/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use auth_consts::AUTH_SET;
use auth_consts::DATA_PROJECT;
use auth_consts::INTERN_CONTROLLER;
use auth_consts::ONCALL;
use auth_consts::ONCALL_GROUP_TYPE;
use auth_consts::REPO;
use auth_consts::SANDCASTLE_CMD;
use auth_consts::SANDCASTLE_TAG;
use auth_consts::SERVICE_IDENTITY;
#[cfg(fbcode_build)]
use configo::ConfigoClient;
use configo::mutation::Mutation;
use configo_thrift_srclients::ConfigoServiceClient;
use configo_thrift_srclients::make_ConfigoService_srclient;
use configo_thrift_srclients::thrift::MutationState;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::try_join_all;
use futures::stream;
use futures_retry::retry;
use git_source_of_truth::GitSourceOfTruth;
use git_source_of_truth::GitSourceOfTruthConfig;
use git_source_of_truth::RepositoryName;
use git_source_of_truth::Staleness;
use git_source_of_truth::flip_landed_mutation_to_mononoke;
use git_symbolic_refs::GitSymbolicRefs;
use git_symbolic_refs::GitSymbolicRefsEntry;
use git_symbolic_refs::SqlGitSymbolicRefsBuilder;
use infrasec_authorization::ACL;
use infrasec_authorization::Identity;
use infrasec_authorization::consts as auth_consts;
use infrasec_authorization_review::AclChange;
use infrasec_authorization_review::AclMetadata;
use infrasec_authorization_review::AclPermissionChange;
use infrasec_authorization_review::ChangeContents;
use infrasec_authorization_review::ChangeOperation;
use infrasec_authorization_review::EntryChange;
use infrasec_authorization_service::CommitChangeSpecificationRequest;
use infrasec_authorization_service_srclients::make_AuthorizationService_srclient;
use infrasec_authorization_service_srclients::thrift::ChangeSpecification;
use infrasec_authorization_service_srclients::thrift::errors::AsNoConfigExistsException;
use itertools::Itertools;
use metaconfig_parser::configerator_repo_spec_handle;
use mononoke_api::MononokeError;
use mononoke_api::RepositoryId;
use mononoke_configs::MononokeConfigs;
use mononoke_macros::mononoke;
use oncall::OncallClient;
use permission_checker::AclProvider;
use repo_authorization::AuthorizationContext;
use repo_spec_writer::DEFAULT_GIT_REPO_SPEC_PATH;
use repo_spec_writer::RepoIndexEntry;
use repo_spec_writer::RepoSpecDir;
use repo_spec_writer::append_to_repo_index;
use repo_spec_writer::make_repo_spec_file_path;
use repo_spec_writer::tier_list_for_repo_spec;
use repos::RawCommitIdentityScheme;
use repos::RawRepoConfig;
use repos::RepoSpec;
use repos::TShirtSize;
use source_control as thrift;
use sql_construct::SqlConstructFromMetadataDatabaseConfig;
use sql_ext::facebook::MysqlOptions;
use thrift::RepoSizeBucket;
use tracing::info;
use tracing::warn;

use crate::source_control_impl::SourceControlServiceImpl;

pub(crate) const DIFF_AUTHOR: &str = "scm_server_infra";
const REPO_SPEC_THRIFT_TYPE: &str = "RepoSpec";
const REPO_SPEC_THRIFT_PATH: &str = "source/scm/mononoke/repos/repos.thrift";
const ORG_GIT_REPO_SPEC_PATH: &str = "scm/mononoke/repos/common/default_org_git_repo_spec";
/// JustKnob gating the attach-to-in-flight-mutation idempotency path in
/// `reserve_repos_ids`. Shared by the production call site and the tests so
/// the two can never drift.
const ATTACH_JK: &str = "scm/mononoke:create_repos_attach_to_inflight_mutation";
/// Whether an out-of-bounds batch is rejected or merely logged. Turning this
/// off is the kill switch; the caps themselves are not runtime-tunable.
/// Switchval'd on the tier name, so enforcement can be turned on one tier at a
/// time.
const ENFORCE_BATCH_SIZE_JK: &str = "scm/mononoke:create_repos_enforce_max_batch_size";
const WRITE_DEFAULT_BRANCH_SYMREF_JK: &str =
    "scm/mononoke:create_repos_write_default_branch_symref";
/// Sequence vs the old `MAX(repo_id) + 1` ceiling. Turning this off reinstates
/// the reuse behind S709055, so it buys diagnosis time, not a resting state.
const ALLOCATE_FROM_SEQUENCE_JK: &str = "scm/mononoke:create_repos_allocate_from_id_sequence";
/// Kill switch for creating repos on org storage engines. Off refuses a
/// dedicated org's creates, never a silent fall back to the template engine:
/// a repo created on the wrong storage cannot be moved.
const STORAGE_CONFIG_JK: &str = "scm/mononoke:create_repos_storage_config_enabled";
/// Org engines are named `org_<slug>`: one the repo portal landed for one
/// org's own bucket and shard. A repo's engine is decided by its org, never by
/// the caller.
pub(crate) const ORG_STORAGE_PREFIX: &str = "org_";

/// The engine one batch is created on, decided before anything is reserved:
/// `None` is the template engine for every repo.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BatchStorage(Option<String>);

impl BatchStorage {
    fn name(&self) -> Option<&str> {
        self.0.as_deref()
    }

    fn repo_spec_template_path(&self) -> &'static str {
        if self.0.is_some() {
            ORG_GIT_REPO_SPEC_PATH
        } else {
            DEFAULT_GIT_REPO_SPEC_PATH
        }
    }
}

/// The batch's engine after validation has passed: every request carries
/// the same value, so the first one is the batch's.
fn batch_storage_name(params: &thrift::CreateReposParams) -> Option<&str> {
    params
        .repos
        .first()
        .and_then(|request| request.storage_config.as_deref())
}

/// The org a repo name belongs to: its first path segment. A bare name has no
/// org and always goes on the template engine.
fn org_of(repo_name: &str) -> Option<&str> {
    repo_name.split_once('/').map(|(slug, _rest)| slug)
}

/// The org -> engine mapping is the loaded storage map: `org_<slug>` present
/// means every repo under `<slug>/` is created on it, absent means the
/// template. `create_org_storage_config` is what adds an entry, so no
/// caller can put a repo on storage its org does not own, and no repo of a
/// dedicated org can land on the template.
fn org_engine_of(
    repo_name: &str,
    storage_configs: &metaconfig_parser::StorageConfigs,
) -> Option<String> {
    let engine = format!("{ORG_STORAGE_PREFIX}{}", org_of(repo_name)?);
    storage_configs
        .storage
        .contains_key(&engine)
        .then_some(engine)
}

/// Decides the batch's engine from the orgs of its repo names, and checks a
/// `storage_config` a request names against it: the field is an assertion
/// (the portal sends the engine it believes the org has, so a stale portal
/// row is refused rather than silently corrected), never a choice. One
/// engine per batch, since the batch shares one symref store and one RepoSpec
/// template. Request errors, so the retry loop reports them instead of
/// retrying.
fn derive_batch_storage(
    repos: &[thrift::RepoCreationRequest],
    storage_configs: &metaconfig_parser::StorageConfigs,
) -> Result<BatchStorage, scs_errors::ServiceError> {
    let derived = repos
        .iter()
        .map(|request| {
            (
                request.repo_name.as_str(),
                org_engine_of(&request.repo_name, storage_configs),
            )
        })
        .collect::<Vec<_>>();
    let engines: HashSet<Option<&str>> = derived
        .iter()
        .map(|(_name, engine)| engine.as_deref())
        .collect();
    if engines.len() > 1 {
        return Err(scs_errors::invalid_request(format!(
            "STORAGE_CONFIG_REFUSED:batch: every repo in a batch must be created on the same \
             storage engine; this batch mixes {}",
            derived
                .iter()
                .map(|(name, engine)| format!(
                    "{name} ({})",
                    engine.as_deref().unwrap_or("template")
                ))
                .join(", "),
        ))
        .into());
    }
    let batch = engines.into_iter().next().flatten().map(str::to_string);
    for (request, (_name, engine)) in repos.iter().zip(derived.iter()) {
        let Some(named) = request.storage_config.as_deref() else {
            continue;
        };
        if Some(named) != engine.as_deref() {
            return Err(scs_errors::invalid_request(format!(
                "STORAGE_CONFIG_REFUSED:org: '{}' asked for storage_config '{named}', but its org \
                 maps to {}; the engine is decided by the org, not the request",
                request.repo_name,
                engine
                    .as_deref()
                    .map(|e| format!("'{e}'"))
                    .unwrap_or_else(|| "the template engine".to_string()),
            ))
            .into());
        }
    }
    Ok(BatchStorage(batch))
}

/// The database every repo config of a tier shares, whatever its engine: the
/// git source-of-truth row and the repo-id sequence live there, and the git
/// server reads them through the repo's own engine while `create_repos`
/// writes them on the service's connection. An engine pointing `production`
/// elsewhere would make a repo whose creation this service can never see.
fn production_db_of(config: &metaconfig_types::StorageConfig) -> Option<String> {
    match &config.metadata {
        metaconfig_types::MetadataDatabaseConfig::Remote(remote) => {
            Some(remote.production.db_address.clone())
        }
        metaconfig_types::MetadataDatabaseConfig::OssRemote(remote) => Some(format!(
            "{}:{}/{}",
            remote.production.host, remote.production.port, remote.production.database
        )),
        metaconfig_types::MetadataDatabaseConfig::Local(local) => {
            Some(local.path.to_string_lossy().into_owned())
        }
    }
}

/// An org engine must share the template's `production` database.
fn check_storage_config_production(
    name: &str,
    template_name: &str,
    storage_configs: &metaconfig_parser::StorageConfigs,
) -> Result<(), scs_errors::ServiceError> {
    let requested = storage_configs.storage.get(name).ok_or_else(|| {
        scs_errors::ServiceError::from(scs_errors::internal_error(format!(
            "Storage config '{name}' was derived from the loaded map and is now missing"
        )))
    })?;
    let template = storage_configs.storage.get(template_name).ok_or_else(|| {
        scs_errors::ServiceError::from(scs_errors::internal_error(format!(
            "Storage config '{template_name}' named by {DEFAULT_GIT_REPO_SPEC_PATH} is not loaded"
        )))
    })?;
    if production_db_of(requested) != production_db_of(template) {
        return Err(scs_errors::invalid_request(format!(
            "STORAGE_CONFIG_REFUSED:production: storage engine '{name}' does not use the same \
             production database as '{template_name}'"
        ))
        .into());
    }
    Ok(())
}

/// Everything about the batch's storage engine that is decided before
/// reservation. The knob refuses a dedicated org's creates outright while
/// off: a repo created on the wrong storage cannot be moved, so there is no
/// silent fall back to the template.
fn validate_storage_configs(
    repos: &[thrift::RepoCreationRequest],
    configs: &MononokeConfigs,
) -> Result<BatchStorage, scs_errors::ServiceError> {
    let storage_configs = configs.storage_configs();
    let storage = derive_batch_storage(repos, &storage_configs)?;
    let Some(name) = storage.name() else {
        return Ok(storage);
    };
    if !justknobs::eval(STORAGE_CONFIG_JK, None, None) {
        return Err(scs_errors::invalid_request(format!(
            "STORAGE_CONFIG_DISABLED: creating repos on org storage engines is switched off \
             ({STORAGE_CONFIG_JK}); '{name}' is this batch's engine"
        ))
        .into());
    }
    let template_name = default_git_storage_name(configs)?;
    check_storage_config_production(name, &template_name, &storage_configs)?;
    Ok(storage)
}

/// The batch's engine written onto every request, so everything downstream
/// (RepoSpec, symrefs, the source-of-truth row) reads one value.
fn with_batch_storage(
    mut params: thrift::CreateReposParams,
    storage: &BatchStorage,
) -> thrift::CreateReposParams {
    for request in &mut params.repos {
        request.storage_config = storage.0.clone();
    }
    params
}

/// Group granting the elevated batch tier, one step below Source Control's own.
/// Membership is the whole mechanism:
/// callers are added and removed there rather than here, so widening or
/// narrowing who may create repos in bulk does not need a diff. Expected
/// members are the automated bundle-import pipeline
/// (`SANDCASTLE_TAG:git_repo_importer`, the tag its Skycastle workflow declares
/// in `required_tag_identities`) and whoever is currently doing bulk onboarding
/// by hand.
const BULK_REPO_CREATORS_GROUP: &str = "bulk_repo_creators";

/// Which batch-size cap applies to a caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BatchSizeTier {
    SourceControl,
    /// Members of the bulk-repo-creators group.
    Elevated,
    Default,
}

impl BatchSizeTier {
    /// Enforcement switchval, and the tier as it appears in the advisory log.
    fn name(self) -> &'static str {
        match self {
            Self::SourceControl => "source_control",
            Self::Elevated => "elevated",
            Self::Default => "default",
        }
    }

    /// Only Source Control's tier covers the largest batches on record. A
    /// delegated grant is deliberately smaller than what the team that hands it
    /// out can do itself -- see the tests, which pin every rejection.
    fn max_batch_size(self) -> usize {
        match self {
            Self::SourceControl => 1000,
            Self::Elevated => 500,
            Self::Default => 50,
        }
    }
}

/// Resolve the caller's batch-size tier.
///
/// A property of the caller, not of the request: the same identity gets the
/// same cap whatever it is creating.
///
/// Elevated is group membership rather than anything hardcoded here, so the
/// set of bulk creators can change without a code change.
///
/// Keyed on the mTLS identity set, deliberately **not** on the `client_id`
/// header: that header is client-supplied and never verified against the
/// identities (`source_control_impl.rs` sets it on the untrusted-proxy branch
/// too), so it cannot carry an authorization decision. It is not keyed on an
/// ACL action either -- a Hipster action expresses a permission, and no ACL
/// carries one for a quota; the closest in-tree precedent for a per-caller
/// limit is a rate-limit target on an identity set
/// (`rate_limiting/src/config.rs`).
///
/// Fails *down*, never up -- a group lookup error yields the lower tier.
async fn batch_size_tier_for_caller(
    ctx: &CoreContext,
    acl_provider: &dyn AclProvider,
) -> BatchSizeTier {
    let identities = ctx.metadata().identities();

    // Checked before the group so that a Source Control member who is also in
    // it gets the higher of the two caps rather than whichever was looked up
    // first. It also means an edit that empties or breaks the group cannot
    // leave the oncall holding an unrecognised caller's cap while they fix it.
    if let Ok(admins) = acl_provider.admin_group().await {
        if admins.is_member(identities).await {
            return BatchSizeTier::SourceControl;
        }
    }

    if let Ok(bulk_creators) = acl_provider.group(BULK_REPO_CREATORS_GROUP).await {
        if bulk_creators.is_member(identities).await {
            return BatchSizeTier::Elevated;
        }
    }

    BatchSizeTier::Default
}

/// Classify a batch against a cap, without deciding whether to act on it.
///
/// The error must be a `Request` error: `create_repos_in_mononoke` retries
/// everything that is not `ServiceError::Request`, so an internal error here
/// would be silently retried instead of reported to the caller.
fn check_batch_size(count: usize, max_batch_size: usize) -> Result<(), scs_errors::ServiceError> {
    if count == 0 {
        return Err(scs_errors::invalid_request(
            "create_repos was called with an empty batch, which creates nothing but still costs a \
             configerator mutation. Omit the call instead."
                .to_string(),
        )
        .into());
    }

    if count <= max_batch_size {
        return Ok(());
    }

    Err(scs_errors::invalid_request(format!(
        "create_repos was asked to create {count} repos, which exceeds the maximum batch size of \
         {max_batch_size} for this caller. Split the request into smaller batches, or ask Source \
         Control for membership of the '{BULK_REPO_CREATORS_GROUP}' group."
    ))
    .into())
}

const HEAD_SYMREF: &str = "HEAD";

fn validate_default_branch(branch: &str) -> Result<&str, scs_errors::ServiceError> {
    if branch.is_empty() {
        return Err(scs_errors::invalid_request(
            "default_branch must not be empty; omit the field to create a repo without a HEAD \
             symref"
                .to_string(),
        )
        .into());
    }
    if branch.eq_ignore_ascii_case(HEAD_SYMREF) {
        return Err(scs_errors::invalid_request(
            "default_branch must not be 'HEAD' (in any casing): HEAD is the symref that points \
             at the default branch, not a branch itself"
                .to_string(),
        )
        .into());
    }
    if branch.starts_with("refs/") {
        return Err(scs_errors::invalid_request(format!(
            "default_branch must be a short branch name (e.g. 'main'), not a full ref: '{branch}'"
        ))
        .into());
    }
    Ok(branch)
}

/// Gate for the `default_branch` feature, evaluated once per batch: returns
/// whether symref writes are enabled, validating every set `default_branch`
/// when they are. The JK is never evaluated when no request sets the field,
/// and with it off the field is fully inert (no validation, no writes).
fn validate_default_branches(
    repos: &[thrift::RepoCreationRequest],
) -> Result<bool, scs_errors::ServiceError> {
    if !repos.iter().any(|request| request.default_branch.is_some()) {
        return Ok(false);
    }
    if !justknobs::eval(WRITE_DEFAULT_BRANCH_SYMREF_JK, None, None) {
        return Ok(false);
    }
    for request in repos {
        if let Some(branch) = &request.default_branch {
            validate_default_branch(branch)?;
        }
    }
    Ok(true)
}

async fn write_default_branch_symref(
    ctx: &CoreContext,
    store: &dyn GitSymbolicRefs,
    branch: &str,
) -> Result<(), scs_errors::ServiceError> {
    let entry = GitSymbolicRefsEntry::new(
        HEAD_SYMREF.to_string(),
        branch.to_string(),
        "branch".to_string(),
    )
    .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    store
        .add_or_update_entries(ctx, vec![entry])
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    Ok(())
}

/// A symref lives on the engine the repo will read it from: the one its
/// `create_repos` request named, or the template's. The engine travels with
/// the row (`GitSourceOfTruthConfigEntry::storage_config`), so a cleanup on
/// another host opens the same store the reservation wrote to.
#[async_trait::async_trait]
trait SymrefStoreProvider: Send + Sync {
    async fn repo_store(
        &self,
        repo_id: RepositoryId,
        storage_config: Option<&str>,
    ) -> Result<Arc<dyn GitSymbolicRefs>, scs_errors::ServiceError>;
}

#[derive(Clone)]
struct SymrefStoreFactory {
    fb: FacebookInit,
    configs: Arc<MononokeConfigs>,
    mysql_options: MysqlOptions,
    /// One builder per engine, opened on first use; per-repo stores on the
    /// same engine share its connections. Keyed by the engine name, with the
    /// template's under `None`.
    builders: Arc<tokio::sync::Mutex<HashMap<Option<String>, SqlGitSymbolicRefsBuilder>>>,
}

/// The storage config a repo id will get once it has one. Every store
/// `create_repos` opens is keyed by an id that is reserved but not yet landed,
/// so it is absent from `repo_configs` by definition and the default git
/// RepoSpec template is the only thing that can name its database.
fn default_git_storage_name(configs: &MononokeConfigs) -> Result<String, scs_errors::ServiceError> {
    let template = load_git_repo_spec_template(configs, DEFAULT_GIT_REPO_SPEC_PATH)?;
    template_storage_name(&template)
}

/// Load a repo template and refuse it unless it is usable; every
/// caller gets a validated template or an error, never a half-usable one.
fn load_git_repo_spec_template(
    configs: &MononokeConfigs,
    template_path: &str,
) -> Result<Arc<RepoSpec>, scs_errors::ServiceError> {
    let config_store = configs.config_store().ok_or_else(|| {
        scs_errors::internal_error("No config store available for loading a git repo spec template")
    })?;
    let template = configerator_repo_spec_handle(template_path, config_store)
        .map_err(|e| {
            scs_errors::internal_error(format!(
                "Failed to load repo spec template '{template_path}': {e:#}"
            ))
        })?
        .get();
    validate_template(&template)?;
    Ok(template)
}

/// The template is embedded verbatim into every new repo, so a bad template
/// is a bad repo. Checked before any Configo write: `create_repos` is
/// git-only (see `RepoSpecDir`), the symref store needs the storage config
/// before the repo's own config exists, and a repo with no tiers is served by
/// nothing.
fn validate_template(template: &RepoSpec) -> Result<(), scs_errors::ServiceError> {
    if template.default_commit_identity_scheme != RawCommitIdentityScheme::GIT {
        return Err(scs_errors::internal_error(format!(
            "Template '{}' has default_commit_identity_scheme {:?}; \
             create_repos only creates GIT repos",
            template.repo_name, template.default_commit_identity_scheme
        ))
        .into());
    }
    template_storage_name(template)?;
    if template.tiers.is_empty() {
        return Err(scs_errors::internal_error(format!(
            "Template '{}' lists no tiers",
            template.repo_name
        ))
        .into());
    }
    Ok(())
}

fn template_storage_name(template: &RepoSpec) -> Result<String, scs_errors::ServiceError> {
    template
        .repo_config
        .as_ref()
        .and_then(|c| c.storage_config.clone())
        .ok_or_else(|| {
            scs_errors::internal_error(format!(
                "Template '{}' does not name a storage config",
                template.repo_name
            ))
            .into()
        })
}

/// Open a metadata store against the named storage config, or the template's
/// when none is named. `store_name` names the store in the error text;
/// without it two callers opening different stores against the same config
/// produce the same message.
async fn open_git_metadata_store<T>(
    fb: FacebookInit,
    configs: &MononokeConfigs,
    mysql_options: &MysqlOptions,
    storage: Option<&str>,
    store_name: &str,
) -> Result<T, scs_errors::ServiceError>
where
    T: SqlConstructFromMetadataDatabaseConfig,
{
    let storage_name = match storage {
        Some(name) => name.to_string(),
        None => default_git_storage_name(configs)?,
    };
    let storage_configs = configs.storage_configs();
    let storage_config = storage_configs.storage.get(&storage_name).ok_or_else(|| {
        scs_errors::ServiceError::from(scs_errors::internal_error(format!(
            "Storage config '{storage_name}' not found while building the {store_name} store"
        )))
    })?;
    T::with_metadata_database_config(fb, &storage_config.metadata, mysql_options, false)
        .await
        .map_err(|e| {
            scs_errors::ServiceError::from(scs_errors::internal_error(format!(
                "Failed to open the {store_name} store: {e:#}"
            )))
        })
}

impl SymrefStoreFactory {
    async fn builder_for(
        &self,
        storage_config: Option<&str>,
    ) -> Result<SqlGitSymbolicRefsBuilder, scs_errors::ServiceError> {
        let key = storage_config.map(str::to_string);
        let mut builders = self.builders.lock().await;
        if let Some(builder) = builders.get(&key) {
            return Ok(builder.clone());
        }
        let builder: SqlGitSymbolicRefsBuilder = open_git_metadata_store(
            self.fb,
            &self.configs,
            &self.mysql_options,
            storage_config,
            "symref",
        )
        .await?;
        builders.insert(key, builder.clone());
        Ok(builder)
    }
}

#[async_trait::async_trait]
impl SymrefStoreProvider for SymrefStoreFactory {
    async fn repo_store(
        &self,
        repo_id: RepositoryId,
        storage_config: Option<&str>,
    ) -> Result<Arc<dyn GitSymbolicRefs>, scs_errors::ServiceError> {
        let builder = self.builder_for(storage_config).await?;
        Ok(Arc::new(builder.build(repo_id)))
    }
}

/// `enabled` is the batch-wide decision from `validate_default_branches`;
/// the JK must not be re-evaluated here.
async fn write_default_branch_symrefs(
    ctx: &CoreContext,
    symref_store_provider: &dyn SymrefStoreProvider,
    repo_ids_and_requests: &[(RepositoryId, thrift::RepoCreationRequest)],
    enabled: bool,
) -> Result<Vec<RepositoryId>, scs_errors::ServiceError> {
    // Validated before reservation: every request in the batch names the
    // same engine, so the first one's is the batch's.
    let storage = repo_ids_and_requests
        .first()
        .and_then(|(_repo_id, request)| request.storage_config.clone());
    if !enabled {
        return Ok(Vec::new());
    }
    let targets = repo_ids_and_requests
        .iter()
        .filter_map(|(repo_id, request)| {
            request
                .default_branch
                .clone()
                .map(|branch| (*repo_id, branch))
        })
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    let repo_ids = targets
        .iter()
        .map(|(repo_id, _branch)| *repo_id)
        .collect::<Vec<_>>();
    stream::iter(targets)
        .map(|(repo_id, branch)| {
            let storage = storage.clone();
            async move {
                let store = symref_store_provider
                    .repo_store(repo_id, storage.as_deref())
                    .await?;
                write_default_branch_symref(ctx, store.as_ref(), &branch).await
            }
        })
        .buffer_unordered(10)
        .try_collect::<Vec<()>>()
        .await?;

    Ok(repo_ids)
}

/// `repos` pairs each id with the engine its row records.
async fn delete_head_symrefs(
    ctx: &CoreContext,
    symref_store_provider: &dyn SymrefStoreProvider,
    repos: &[(RepositoryId, Option<String>)],
) -> Result<(), scs_errors::ServiceError> {
    stream::iter(repos.iter().cloned())
        .map(|(repo_id, storage_config)| async move {
            let store = symref_store_provider
                .repo_store(repo_id, storage_config.as_deref())
                .await?;
            store
                .delete_symrefs(ctx, vec![HEAD_SYMREF.to_string()])
                .await
                .map_err(|e| {
                    scs_errors::ServiceError::from(scs_errors::internal_error(format!(
                        "Failed to delete the HEAD symref for repo {repo_id}: {e:#}"
                    )))
                })
        })
        .buffer_unordered(10)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

async fn cleanup_reserved_repos_after_failure(
    ctx: &CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    symref_store_provider: &dyn SymrefStoreProvider,
    symref_repo_ids: &[RepositoryId],
    params: &thrift::CreateReposParams,
) -> Result<(), scs_errors::ServiceError> {
    // The rows were written by this call from a validated batch, so the
    // batch's engine is theirs.
    let storage = batch_storage_name(params).map(str::to_string);
    let symref_repos = symref_repo_ids
        .iter()
        .map(|repo_id| (*repo_id, storage.clone()))
        .collect::<Vec<_>>();
    // Symref deletes go before the SoT deletes so a failure leaves the rows
    // Reserved and retryable; same retry shape as the poll-path cleanup.
    retry(
        |_| delete_head_symrefs(ctx, symref_store_provider, &symref_repos),
        Duration::from_millis(1_000),
    )
    .binary_exponential_backoff()
    .max_attempts(5)
    .await?;
    retry(
        |_| {
            delete_source_of_truth_for_reserved_repos(
                ctx.clone(),
                git_source_of_truth_config,
                params,
            )
        },
        Duration::from_millis(1_000),
    )
    .binary_exponential_backoff()
    .max_attempts(5)
    .await?;
    Ok(())
}

async fn ensure_acls_allow_repo_creation(
    ctx: CoreContext,
    repos: &[thrift::RepoCreationRequest],
    acl_provider: &dyn AclProvider,
) -> Result<(), scs_errors::ServiceError> {
    let authz = AuthorizationContext::new(&ctx);
    try_join_all(
        repos
            .iter()
            .map(|repo| authz.require_repo_create(&ctx, &repo.repo_name, acl_provider)),
    )
    .await
    .map_err(Into::<MononokeError>::into)?;
    Ok(())
}

#[cfg(fbcode_build)]
fn hipster_identity(id_type: &str, id_data: &str) -> Identity {
    Identity {
        id_type: id_type.to_string(),
        id_data: id_data.to_string(),
        ..Default::default()
    }
}

#[cfg(fbcode_build)]
fn initial_acl_grants(maintainers: &[Identity], writers: &[Identity]) -> Vec<AclPermissionChange> {
    let standard = |identities: &[(&str, &str)]| -> Vec<Identity> {
        identities
            .iter()
            .map(|(id_type, id_data)| hipster_identity(id_type, id_data))
            .collect()
    };
    [
        (
            "read",
            standard(&[
                (AUTH_SET, "cocomatic_service_identities"),
                (AUTH_SET, "coding_crewmates"),
                (AUTH_SET, "svcscm_read_all"),
                (AUTH_SET, "svnuser"),
                (INTERN_CONTROLLER, "XInternGraphGraphQLController"),
                (SERVICE_IDENTITY, "aosp_megarepo_service_identity"),
                (SERVICE_IDENTITY, "gitremoteimport"),
                (SERVICE_IDENTITY, "scm_service_identity"),
                (SANDCASTLE_TAG, "skycastle_gitimport"),
                (SANDCASTLE_TAG, "tpms_sandcastle_tag"),
                (SANDCASTLE_CMD, "SandcastleLandCommand"),
                (SANDCASTLE_CMD, "SandcastlePushCommand"),
            ]),
        ),
        (
            "write",
            [
                standard(&[
                    (AUTH_SET, "svnuser"),
                    (SANDCASTLE_CMD, "SandcastleLandCommand"),
                    (SANDCASTLE_CMD, "SandcastlePushCommand"),
                ]),
                writers.to_vec(),
            ]
            .concat(),
        ),
        (
            "bypass_readonly",
            standard(&[(AUTH_SET, "scm"), (SERVICE_IDENTITY, "gitremoteimport")]),
        ),
        ("maintainers", maintainers.to_vec()),
    ]
    .into_iter()
    .map(|(action, identities)| AclPermissionChange {
        action: action.to_string(),
        operation: ChangeOperation::ADD,
        entry_changes: identities
            .into_iter()
            .map(|entry| EntryChange {
                entry,
                operation: ChangeOperation::ADD,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    })
    .collect()
}

/// The identity the GitHub mirror sync runs as.
#[cfg(fbcode_build)]
const GITHUB_MIRROR_SYNC_TAG: &str = "scm_github_mirror_sync";
/// What the sync needs on its repo's ACL: gitimport pushes into a read-only
/// repo with hooks bypassed.
#[cfg(fbcode_build)]
const GITHUB_MIRROR_SYNC_ACTIONS: [&str; 4] =
    ["read", "write", "write_no_hooks", "bypass_readonly"];
/// The data project the mirror's verifier sweep reads as. The sweep dispatches
/// a new mirror's first sync and reconciles webhooks GitHub never delivered.
#[cfg(fbcode_build)]
const GITHUB_MIRROR_VERIFIER_DATA_PROJECT: &str = "scm";
/// Hipster commits one ACL per call, so a batch of mirrors with their own ACLs
/// is bounded rather than sequential or unbounded.
#[cfg(fbcode_build)]
const GITHUB_MIRROR_ACL_CONCURRENCY: usize = 10;

fn github_mirror_maintainer_oncall(request: &thrift::RepoCreationRequest) -> Option<&str> {
    request.github_mirror.as_ref()?.maintainer_oncall.as_deref()
}

/// Hipster's group for an oncall rotation.
#[cfg(fbcode_build)]
fn oncall_group_identity(oncall: &str) -> Identity {
    hipster_identity(ONCALL_GROUP_TYPE, &format!("oncall_{oncall}"))
}

/// Every (action, identity) the sync and the verifier sweep need on the ACL
/// the repo uses.
#[cfg(fbcode_build)]
fn github_mirror_sync_entries() -> Vec<(&'static str, Identity)> {
    GITHUB_MIRROR_SYNC_ACTIONS
        .iter()
        .map(|action| {
            (
                *action,
                hipster_identity(SANDCASTLE_TAG, GITHUB_MIRROR_SYNC_TAG),
            )
        })
        .chain([(
            "read",
            hipster_identity(DATA_PROJECT, GITHUB_MIRROR_VERIFIER_DATA_PROJECT),
        )])
        .collect()
}

/// Every (action, identity) a GitHub mirror's own ACL must hold.
#[cfg(fbcode_build)]
fn required_github_mirror_entries(oncall: &str) -> Vec<(&'static str, Identity)> {
    github_mirror_sync_entries()
        .into_iter()
        .chain([("read", oncall_group_identity(oncall))])
        .collect()
}

/// Source Control maintains the ACL alongside the oncall, so a broken sync
/// grant can be fixed without the oncall.
#[cfg(fbcode_build)]
fn github_mirror_acl_grants(oncall: &str) -> Vec<AclPermissionChange> {
    let maintainers = [
        oncall_group_identity(oncall),
        hipster_identity(AUTH_SET, "scm"),
    ];
    with_entries(
        initial_acl_grants(&maintainers, &[]),
        required_github_mirror_entries(oncall),
    )
}

/// `grants` plus each (action, identity), under the action's existing grant
/// when there is one.
#[cfg(fbcode_build)]
fn with_entries(
    grants: Vec<AclPermissionChange>,
    entries: Vec<(&'static str, Identity)>,
) -> Vec<AclPermissionChange> {
    entries
        .into_iter()
        .fold(grants, |mut grants, (action, entry)| {
            let entry = EntryChange {
                entry,
                operation: ChangeOperation::ADD,
                ..Default::default()
            };
            match grants.iter_mut().find(|grant| grant.action == action) {
                Some(grant) => grant.entry_changes.push(entry),
                None => grants.push(AclPermissionChange {
                    action: action.to_string(),
                    operation: ChangeOperation::ADD,
                    entry_changes: vec![entry],
                    ..Default::default()
                }),
            }
            grants
        })
}

/// The required entries an existing ACL lacks, as `action:TYPE:data`.
#[cfg(fbcode_build)]
fn missing_github_mirror_entries(acl: &ACL, oncall: &str) -> Vec<String> {
    required_github_mirror_entries(oncall)
        .into_iter()
        .filter(|(action, identity)| {
            !acl.permissions.iter().any(|permission| {
                permission.action == *action
                    && permission.entries.iter().any(|entry| {
                        entry.identity.id_type == identity.id_type
                            && entry.identity.id_data == identity.id_data
                    })
            })
        })
        .map(|(action, identity)| format!("{action}:{}:{}", identity.id_type, identity.id_data))
        .collect()
}

#[cfg(fbcode_build)]
fn make_initial_acl_creation_request(
    acl_name: &str,
    oncall_name: &str,
    grants: Vec<AclPermissionChange>,
) -> CommitChangeSpecificationRequest {
    let repo_group = hipster_identity(REPO, acl_name);
    let acl_change = AclChange {
        acl: repo_group,
        permission_changes: grants,
        operation: ChangeOperation::ADD,
        metadata_update: Some(AclMetadata {
            oncall: Some(oncall_name.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let change_contents = ChangeContents {
        acl_changes: vec![acl_change],
        ..Default::default()
    };
    let spec = ChangeSpecification::contents(change_contents);
    let reason = "automated repo creation".to_string();
    CommitChangeSpecificationRequest {
        spec,
        commit_message: reason,
        ..Default::default()
    }
}

#[cfg(fbcode_build)]
async fn create_repo_acl(
    ctx: CoreContext,
    acl_name: &str,
    oncall_name: &str,
    grants: Vec<AclPermissionChange>,
) -> Result<(), scs_errors::ServiceError> {
    let request = make_initial_acl_creation_request(acl_name, oncall_name, grants);
    let thrift_client = make_AuthorizationService_srclient!(ctx.fb)
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    thrift_client
        .commitChangeSpecification(&request)
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    Ok(())
}

#[cfg(fbcode_build)]
async fn is_valid_oncall_name(
    ctx: CoreContext,
    oncall_name: &str,
    valid_oncall_names_cache: &mut HashSet<String>,
) -> Result<bool, scs_errors::ServiceError> {
    // Check the cache first
    if valid_oncall_names_cache.contains(oncall_name) {
        Ok(true)
    } else {
        // Sanity check the syntax to avoid making an unnecessary call to the oncall service
        if oncall_name
            .chars()
            .any(|c| !(c.is_ascii_digit() || c.is_ascii_lowercase() || c == '_'))
        {
            return Ok(false);
        }
        // Validate the oncall actually exists
        match OncallClient::new(ctx.fb)
            .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?
            .get_current_oncall(oncall_name)
            .await
        {
            Ok(_) => {
                // Cache the successful result to avoid unnecessary calls in the future
                valid_oncall_names_cache.insert(oncall_name.to_string());
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

#[cfg(fbcode_build)]
async fn is_valid_hipster_group(
    ctx: CoreContext,
    hipster_group: &str,
    valid_hipster_groups_cache: &mut HashSet<String>,
) -> Result<bool, scs_errors::ServiceError> {
    // Check the cache first
    if valid_hipster_groups_cache.contains(hipster_group) {
        Ok(true)
    } else {
        // Sanity check the syntax to avoid making an unnecessary call to the oncall service
        if hipster_group
            .chars()
            .any(|c| !(c.is_ascii_digit() || c.is_ascii_lowercase() || c == '_'))
        {
            return Ok(false);
        }
        // Validate the hipster group actually exists
        let thrift_client = make_AuthorizationService_srclient!(ctx.fb)
            .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
        let exists = thrift_client
            .groupExists(hipster_group)
            .await
            .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
        if exists {
            // Cache the successful result to avoid unnecessary calls in the future
            valid_hipster_groups_cache.insert(hipster_group.to_string());
        }
        Ok(exists)
    }
}

#[cfg(fbcode_build)]
async fn validate_repo_acl(
    acl: ACL,
    acl_name: &str,
    oncall_name: &str,
    hipster_group: &str,
) -> Result<(), scs_errors::ServiceError> {
    // Ensure this hipster group (as AUTH_SET or ONCALL_GROUP type) is a maintainer for this ACL
    if !acl.permissions.iter().any(|permission| {
        permission.action == "maintainers"
            && permission.entries.iter().any(|entry| {
                entry.identity.id_data == hipster_group
                    && (entry.identity.id_type == AUTH_SET
                        || entry.identity.id_type == ONCALL
                        || entry.identity.id_type == ONCALL_GROUP_TYPE)
            })
    }) {
        return Err(scs_errors::invalid_request(format!(
            "Hipster group: {hipster_group} is not a maintainer for acl: {acl_name}"
        ))
        .into());
    }
    // Ensure this oncall is point of contact for this ACL
    if acl.point_of_contact.id_data != oncall_name {
        return Err(scs_errors::invalid_request(format!(
            "Oncall: {oncall_name} is not a point of contact for acl: {acl_name}"
        ))
        .into());
    }
    Ok(())
}

#[cfg(fbcode_build)]
async fn try_fetching_repo_acl(
    ctx: CoreContext,
    acl_name: &str,
) -> Result<Option<ACL>, scs_errors::ServiceError> {
    let thrift_client = make_AuthorizationService_srclient!(ctx.fb)
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;

    let repo_group = Identity {
        id_type: REPO.to_string(),
        id_data: acl_name.to_string(),
        ..Default::default()
    };
    match thrift_client.getAuthConfig(&repo_group).await {
        Err(e) => {
            if e.as_no_config_exists_exception().is_some() {
                Ok(None)
            } else {
                Err(scs_errors::internal_error(format!("{e:#}")).into())
            }
        }
        Ok(auth_config) => Ok(Some(auth_config.acl)),
    }
}

/// Build a Hipster ACL name for a repo, lowercased.
///
/// Hipster auto-lowercases ACL names on write (visible in `consumer.id_data`
/// from `hipstercli getrawacl`). Mononoke's per-permission push-time check
/// (e.g. the `BYPASS_ALL_HOOKS` pushvar check on `write_no_hooks`) is
/// case-sensitive against the ACL name stored on the repo, so without
/// normalization a repo whose name has uppercase characters ends up
/// configured with an ACL name that no Hipster entry matches.
///
/// Concrete failure mode: XF-APAC/dreamwright-v2 mirror sync, 2026-06-24
/// 16:44 UTC. The Mononoke repo was configured with
/// `custom_acl_name="repos/git/XF-APAC"` but the actual Hipster entry was
/// `repos/git/xf-apac`. Every `gitimport --bypass-all-hooks` push to it
/// failed with "needs … write_no_hooks action on repo ACL" because the
/// case-sensitive grant lookup missed. par-msl was unaffected only because
/// its org slug was already lowercase.
///
/// Lowercasing here keeps Mononoke's stored ACL name byte-equal to what
/// Hipster will return for the same logical ACL, for every tenant
/// regardless of how their org slug is cased on github.com.
fn make_full_acl_name_from_repo_name(repo_name: &str) -> String {
    format!("repos/git/{}", repo_name.to_lowercase())
}

fn make_top_level_acl_name_from_repo_name(repo_name: &str) -> String {
    // IMPORTANT: this hardcodes "repos/git/" because create_repos only supports GIT today.
    // Hg repos use "repos/hg/<name>" ACLs (e.g., "repos/hg/aosp"). When adding HG support
    // to create_repos, this function must branch on identity_scheme — see the
    // _IDENTITY_SUBDIR mapping in configerator/source/scm/mononoke/repos/generate_repo_index.py.
    // NOTE for future implementer: any logging added inside add_repo() must use debug! not info!
    // — info! in add_repo() breaks .t integration tests (project memory).
    //
    // Case normalization: see `make_full_acl_name_from_repo_name` docstring
    // for the rationale (XF-APAC mirror sync SEV, 2026-06-24).
    let (top_level, _rest) = repo_name.split_once('/').unwrap_or((repo_name, ""));
    format!("repos/git/{}", top_level.to_lowercase())
}

#[cfg(fbcode_build)]
async fn validate_and_process_custom_acl(
    ctx: CoreContext,
    repo_creation_request: &thrift::RepoCreationRequest,
    custom_acl: &thrift::CustomAclParams,
    valid_oncall_names_cache: &mut HashSet<String>,
    valid_hipster_groups_cache: &mut HashSet<String>,
) -> Result<(), scs_errors::ServiceError> {
    let acl_name = make_full_acl_name_from_repo_name(&repo_creation_request.repo_name);

    if !is_valid_oncall_name(
        ctx.clone(),
        &repo_creation_request.oncall_name,
        valid_oncall_names_cache,
    )
    .await?
    {
        return Err(scs_errors::invalid_request(format!(
            "Invalid oncall name: {}",
            repo_creation_request.oncall_name
        ))
        .into());
    }

    if !is_valid_hipster_group(
        ctx.clone(),
        &custom_acl.hipster_group,
        valid_hipster_groups_cache,
    )
    .await?
    {
        return Err(scs_errors::invalid_request(format!(
            "Invalid hipster group: {}",
            custom_acl.hipster_group
        ))
        .into());
    }

    if let Some(acl) = try_fetching_repo_acl(ctx.clone(), &acl_name).await? {
        validate_repo_acl(
            acl,
            &acl_name,
            &repo_creation_request.oncall_name,
            &custom_acl.hipster_group,
        )
        .await?;
    } else {
        create_repo_acl(
            ctx,
            &acl_name,
            &repo_creation_request.oncall_name,
            initial_acl_grants(
                &[hipster_identity(AUTH_SET, &custom_acl.hipster_group)],
                &[],
            ),
        )
        .await?;
    }

    Ok(())
}

#[cfg(fbcode_build)]
fn to_hipster_identity(identity: &thrift::AclIdentity) -> Identity {
    hipster_identity(&identity.id_type, &identity.id_data)
}

/// The ACL shape checks that need no Hipster call, run over the whole batch
/// before any ACL is created so an invalid request has no side effects.
#[cfg(fbcode_build)]
fn validate_acl_request_shape(
    request: &thrift::RepoCreationRequest,
) -> Result<(), scs_errors::ServiceError> {
    let Some(org_acl) = &request.org_acl else {
        return Ok(());
    };
    if request.custom_acl.is_some() {
        return Err(scs_errors::invalid_request(format!(
            "{}: custom_acl and org_acl cannot both be set",
            request.repo_name
        ))
        .into());
    }
    if org_acl.maintainers.is_empty() {
        return Err(scs_errors::invalid_request(format!(
            "{}: org_acl needs at least one maintainer",
            request.repo_name
        ))
        .into());
    }
    Ok(())
}

/// A mirror's ACL is decided by `github_mirror` alone. ACLs are created before
/// the repos, so a batch that fails partway leaves ACLs for repos that do not
/// exist yet; a retry accepts them, because `ensure_github_mirror_acl` takes an
/// existing ACL that already holds the mirror's entries.
#[cfg(fbcode_build)]
fn validate_github_mirror_request_shape(
    request: &thrift::RepoCreationRequest,
) -> Result<(), scs_errors::ServiceError> {
    let Some(github_mirror) = &request.github_mirror else {
        return Ok(());
    };
    let refusal = if request.custom_acl.is_some() {
        "github_mirror cannot be combined with custom_acl"
    } else if request.org_acl.is_some() && github_mirror.maintainer_oncall.is_some() {
        "github_mirror.maintainer_oncall gives the repo its own ACL, so org_acl does not apply"
    } else if request.readonly == Some(false) {
        "a GitHub mirror is always read-only; omit readonly"
    } else {
        return Ok(());
    };
    Err(scs_errors::invalid_request(format!("{}: {refusal}", request.repo_name)).into())
}

/// Create the mirror's own ACL, or accept an existing one only if it already
/// lets the sync write and the oncall read: anything less would create a repo
/// that never syncs. The caller has checked that `oncall` exists.
#[cfg(fbcode_build)]
async fn ensure_github_mirror_acl(
    ctx: CoreContext,
    repo_creation_request: &thrift::RepoCreationRequest,
    oncall: &str,
) -> Result<(), scs_errors::ServiceError> {
    let acl_name = make_full_acl_name_from_repo_name(&repo_creation_request.repo_name);
    let Some(acl) = try_fetching_repo_acl(ctx.clone(), &acl_name).await? else {
        return create_repo_acl(ctx, &acl_name, oncall, github_mirror_acl_grants(oncall)).await;
    };
    let missing = missing_github_mirror_entries(&acl, oncall);
    if missing.is_empty() {
        return Ok(());
    }
    Err(scs_errors::invalid_request(format!(
        "ACL {acl_name} already exists but lacks {}",
        missing.join(", ")
    ))
    .into())
}

/// The grants of a new org ACL. A mirror inherits it, so it also carries the
/// sync's grants: without them the repos are created and never sync.
#[cfg(fbcode_build)]
fn org_acl_grants(
    maintainers: &[Identity],
    writers: &[Identity],
    is_mirror: bool,
) -> Vec<AclPermissionChange> {
    let grants = initial_acl_grants(maintainers, writers);
    if is_mirror {
        with_entries(grants, github_mirror_sync_entries())
    } else {
        grants
    }
}

/// The repo inherits `repos/git/<org>`; create it from `org_acl` when it is
/// missing, else refuse as before. A concurrent first repo in the same org
/// can win the create; the ACL being there afterwards is what matters.
#[cfg(fbcode_build)]
async fn ensure_top_level_acl(
    ctx: CoreContext,
    repo_creation_request: &thrift::RepoCreationRequest,
    valid_oncall_names_cache: &mut HashSet<String>,
    valid_hipster_groups_cache: &mut HashSet<String>,
) -> Result<(), scs_errors::ServiceError> {
    let acl_name = make_top_level_acl_name_from_repo_name(&repo_creation_request.repo_name);
    if try_fetching_repo_acl(ctx.clone(), &acl_name)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let Some(org_acl) = &repo_creation_request.org_acl else {
        return Err(scs_errors::invalid_request(format!(
            "Top level acl {acl_name} does not exist!"
        ))
        .into());
    };
    if !is_valid_oncall_name(ctx.clone(), &org_acl.oncall_name, valid_oncall_names_cache).await? {
        return Err(scs_errors::invalid_request(format!(
            "Invalid oncall name: {}",
            org_acl.oncall_name
        ))
        .into());
    }
    let maintainers: Vec<Identity> = org_acl
        .maintainers
        .iter()
        .map(to_hipster_identity)
        .collect();
    let writers: Vec<Identity> = org_acl.writers.iter().map(to_hipster_identity).collect();
    for group in maintainers
        .iter()
        .chain(&writers)
        .filter(|identity| identity.id_type == AUTH_SET)
    {
        if !is_valid_hipster_group(ctx.clone(), &group.id_data, valid_hipster_groups_cache).await? {
            return Err(scs_errors::invalid_request(format!(
                "Invalid hipster group: {}",
                group.id_data
            ))
            .into());
        }
    }
    let grants = org_acl_grants(
        &maintainers,
        &writers,
        repo_creation_request.github_mirror.is_some(),
    );
    if let Err(e) = create_repo_acl(ctx.clone(), &acl_name, &org_acl.oncall_name, grants).await
        && try_fetching_repo_acl(ctx, &acl_name).await?.is_none()
    {
        return Err(e);
    }
    Ok(())
}

/// Ensure all repos have the necessary ACLs set-up.
/// Note: Currently, this duplicates the logic that happens when creating the repositories in
/// Metagit by forking to `scmadmin` in `create_repos_in_metagit`, but this will go away
/// eventually, so we need this to happen here to prepare for that
#[cfg(fbcode_build)]
async fn update_repos_acls(
    ctx: CoreContext,
    params: &thrift::CreateReposParams,
) -> Result<(), scs_errors::ServiceError> {
    params.repos.iter().try_for_each(|request| {
        validate_acl_request_shape(request)?;
        validate_github_mirror_request_shape(request)
    })?;

    let mut valid_oncall_names_cache = HashSet::new();
    let mut valid_hipster_groups_cache = HashSet::new();

    let mirrors_with_own_acl: Vec<_> = params
        .repos
        .iter()
        .filter_map(|request| {
            github_mirror_maintainer_oncall(request).map(|oncall| (request, oncall))
        })
        .collect();
    let mirror_oncalls: HashSet<&str> = mirrors_with_own_acl
        .iter()
        .map(|(_, oncall)| *oncall)
        .collect();
    for oncall in mirror_oncalls {
        if !is_valid_oncall_name(ctx.clone(), oncall, &mut valid_oncall_names_cache).await? {
            return Err(
                scs_errors::invalid_request(format!("Invalid oncall name: {oncall}")).into(),
            );
        }
    }
    // Built up front: a stream that maps borrowed requests to futures is not
    // `Send` for every lifetime the thrift handler needs.
    let mirror_acls: Vec<_> = mirrors_with_own_acl
        .into_iter()
        .map(|(request, oncall)| ensure_github_mirror_acl(ctx.clone(), request, oncall))
        .collect();
    stream::iter(mirror_acls)
        .buffer_unordered(GITHUB_MIRROR_ACL_CONCURRENCY)
        .try_collect::<()>()
        .await?;

    for repo_creation_request in params
        .repos
        .iter()
        .filter(|request| github_mirror_maintainer_oncall(request).is_none())
    {
        if let Some(custom_acl) = &repo_creation_request.custom_acl {
            validate_and_process_custom_acl(
                ctx.clone(),
                repo_creation_request,
                custom_acl,
                &mut valid_oncall_names_cache,
                &mut valid_hipster_groups_cache,
            )
            .await?;
        } else {
            ensure_top_level_acl(
                ctx.clone(),
                repo_creation_request,
                &mut valid_oncall_names_cache,
                &mut valid_hipster_groups_cache,
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(not(fbcode_build))]
async fn update_repos_acls(
    ctx: CoreContext,
    params: &thrift::CreateReposParams,
) -> Result<(), scs_errors::ServiceError> {
    println!("No access to hipster in oss build");
    Ok(())
}

/// Outcome of reserving repo ids for a `create_repos` batch.
#[cfg(fbcode_build)]
#[derive(Debug)]
enum ReserveOutcome {
    /// Repos were freshly reserved; caller must prepare + land a mutation.
    Reserved(Vec<(RepositoryId, thrift::RepoCreationRequest)>),
    /// All requested repos were already `reserved` under a single in-flight
    /// mutation; caller should attach to it and return the token unchanged.
    AttachedToInflight { mutation_id: i64 },
}

/// Pair each requested repo with an id off the sequence. The old
/// `MAX(repo_id) + 1` ceiling rewound whenever `cleanup_repos` deleted rows,
/// reissuing ids that still carried a previous occupant's data (S709055).
#[cfg(fbcode_build)]
/// The pre-sequence allocator, kept verbatim so the kill switch restores the
/// old behaviour exactly, id reuse included.
#[cfg(fbcode_build)]
async fn allocate_repo_ids_from_ceiling(
    ctx: &CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    count: usize,
) -> Result<Vec<RepositoryId>, scs_errors::ServiceError> {
    let max_id = git_source_of_truth_config
        .get_max_id(ctx)
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?
        .ok_or_else(|| {
            scs_errors::internal_error(
                "No rows in git_repositories_source_of_truth. That's unexpected",
            )
        })?;

    let ceiling = max_id.id();
    Ok((1..=count)
        .map(|offset| RepositoryId::new(ceiling + offset as i32))
        .collect())
}

#[cfg(fbcode_build)]
async fn allocate_repo_ids(
    ctx: &CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    params: &thrift::CreateReposParams,
) -> Result<Vec<(RepositoryId, thrift::RepoCreationRequest)>, scs_errors::ServiceError> {
    let repo_ids = if justknobs::eval(ALLOCATE_FROM_SEQUENCE_JK, None, None) {
        git_source_of_truth_config
            .allocate_repo_ids(ctx, params.repos.len())
            .await
            .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?
    } else {
        allocate_repo_ids_from_ceiling(ctx, git_source_of_truth_config, params.repos.len()).await?
    };

    // `zip` would silently drop repos on a short batch and report success.
    if repo_ids.len() != params.repos.len() {
        return Err(scs_errors::internal_error(format!(
            "Allocated {} repo ids for a batch of {} repos",
            repo_ids.len(),
            params.repos.len()
        ))
        .into());
    }

    Ok(repo_ids
        .into_iter()
        .zip(params.repos.iter().cloned())
        .collect())
}

/// Whether `error_trace` is a uniqueness violation on `column` of
/// `git_repositories_source_of_truth`. SQLite names the column and MySQL names
/// the index, which the schema defines as `<column>_idx` for both constraints.
#[cfg(fbcode_build)]
fn violates_unique(error_trace: &str, column: &str) -> bool {
    (error_trace.contains("UNIQUE constraint failed")
        && error_trace.contains(&format!("git_repositories_source_of_truth.{column}")))
        || (error_trace.contains("Duplicate entry")
            && error_trace.contains(&format!("{column}_idx")))
}

#[cfg(fbcode_build)]
async fn reserve_repos_ids(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    params: &thrift::CreateReposParams,
) -> Result<ReserveOutcome, scs_errors::ServiceError> {
    let repo_ids_and_requests = allocate_repo_ids(&ctx, git_source_of_truth_config, params).await?;
    let result = git_source_of_truth_config
        .insert_repos(
            &ctx,
            &repo_ids_and_requests
                .iter()
                .map(|(id, request)| {
                    (
                        id.clone(),
                        RepositoryName(request.repo_name.clone()),
                        GitSourceOfTruth::Reserved,
                        request.storage_config.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .await;
    match result {
        Ok(_) => Ok(ReserveOutcome::Reserved(repo_ids_and_requests)),
        Err(e) => {
            let error_trace = format!("{e:#}");

            // Not the split-brain case below: the sequence hit an id held by a
            // repo outside its bookkeeping, like the hand-assigned ids in the
            // configerator index. Internal, so the retry loop re-allocates; the
            // sequence only moves forward, so a retry clears it.
            if violates_unique(&error_trace, "repo_id") {
                let allocated = repo_ids_and_requests
                    .iter()
                    .map(|(id, _)| id.id().to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                return Err(scs_errors::internal_error(format!(
                    "Repo id sequence issued an id that is already in use (allocated: {allocated}). \
                     Retrying allocates fresh ids. Details: {error_trace}"
                ))
                .into());
            }

            if violates_unique(&error_trace, "repo_name") {
                // Look up every requested repo's current row so we can both
                // build human-readable `details` (today's behavior) and, when
                // the attach knob is enabled, classify whether this is a
                // duplicate request for a single in-flight mutation we can
                // safely attach to.
                let mut details = Vec::new();
                let mut lookups = Vec::with_capacity(repo_ids_and_requests.len());
                for (_id, request) in &repo_ids_and_requests {
                    let repo_name = RepositoryName(request.repo_name.clone());
                    let lookup = git_source_of_truth_config
                        .get_by_repo_name(&ctx, &repo_name, Staleness::MostRecent)
                        .await;
                    match &lookup {
                        Ok(Some(entry)) => match entry.source_of_truth {
                            GitSourceOfTruth::Reserved => {
                                details.push(format!(
                                    "Repo '{}' (id={}) has a stale 'Reserved' entry from a prior failed creation attempt. \
                                     It is safe to delete this row and retry.",
                                    request.repo_name, entry.repo_id
                                ));
                            }
                            ref sot => {
                                details.push(format!(
                                    "DANGER: Repo '{}' (id={}) already exists with source_of_truth={}. \
                                     Do NOT force-create — this will cause split-brain! \
                                     See SEV S617275 for context.",
                                    request.repo_name, entry.repo_id, sot
                                ));
                            }
                        },
                        Ok(None) => {
                            details.push(format!(
                                "Repo '{}': UNIQUE constraint violated but no row found on lookup. \
                                 Original error: {error_trace}",
                                request.repo_name
                            ));
                        }
                        Err(lookup_err) => {
                            details.push(format!(
                                "Repo '{}': UNIQUE constraint violated but lookup failed: {:#}. \
                                 Original error: {error_trace}",
                                request.repo_name, lookup_err
                            ));
                        }
                    }
                    lookups.push(lookup);
                }

                let attach_enabled = justknobs::eval(ATTACH_JK, None, None);

                if attach_enabled {
                    // A lookup `Err` is a transient DB/query failure, NOT a
                    // client-side invalid request. Surface it as an internal
                    // error (mapped to `ServiceError::Internal`) so the retry
                    // loop in `create_repos_in_mononoke` retries it, instead
                    // of masking a retryable failure as `invalid_request`.
                    let lookup_errors = lookups
                        .iter()
                        .filter_map(|lookup| lookup.as_ref().err())
                        .map(|e| format!("{e:#}"))
                        .collect::<Vec<_>>();
                    if !lookup_errors.is_empty() {
                        return Err(scs_errors::internal_error(format!(
                            "Failed to look up reserved repos while classifying a duplicate \
                             creation request: {}",
                            lookup_errors.join("; ")
                        ))
                        .into());
                    }

                    // Only attach when every requested repo resolved to a
                    // `Reserved` row stamped with the SAME mutation_id.
                    let all_reserved_entries = lookups
                        .iter()
                        .map(|lookup| match lookup {
                            Ok(Some(entry))
                                if entry.source_of_truth == GitSourceOfTruth::Reserved =>
                            {
                                entry.mutation_id
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();

                    // All lookups are `Ok` here (errors returned above), so a
                    // non-reserved entry is a genuine split-brain / missing-row
                    // case, not a transient failure.
                    let any_non_reserved = lookups.iter().any(|lookup| {
                        !matches!(
                            lookup,
                            Ok(Some(entry)) if entry.source_of_truth == GitSourceOfTruth::Reserved
                        )
                    });

                    if any_non_reserved {
                        // At least one row is not `Reserved` (or lookup
                        // failed / returned None). If any row is present but
                        // in a non-reserved state, this is the split-brain
                        // case and `details` already carries the DANGER
                        // message. Fall through to the shared error below.
                        return Err(scs_errors::invalid_request(details.join("\n")).into());
                    }

                    // Every row is `Reserved`. The in-flight rows carry the
                    // engine they were reserved on; attaching a request that
                    // names a different one would land its repos on the
                    // other engine and report success.
                    let engine_mismatches = lookups
                        .iter()
                        .zip(repo_ids_and_requests.iter())
                        .filter_map(|(lookup, (_id, request))| match lookup {
                            Ok(Some(entry)) if entry.storage_config != request.storage_config => {
                                Some(format!(
                                    "Repo '{}': in flight on engine {}, this request named {}",
                                    request.repo_name,
                                    entry.storage_config.as_deref().unwrap_or("<template>"),
                                    request.storage_config.as_deref().unwrap_or("<template>"),
                                ))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    if !engine_mismatches.is_empty() {
                        return Err(scs_errors::invalid_request(format!(
                            "STORAGE_CONFIG_REFUSED:inflight: a creation of these repos is already \
                             in progress on a different storage engine; wait for it or let it be \
                             cleaned up.\n{}",
                            engine_mismatches.join("\n")
                        ))
                        .into());
                    }

                    // Decide based on stamping.
                    if all_reserved_entries.iter().any(Option::is_none) {
                        return Err(scs_errors::invalid_request(format!(
                            "Repo creation is already in progress but not yet trackable \
                             (a reserved row has no mutation_id stamped yet); retry shortly, \
                             or delete the stale reserved row if the original attempt died.\n{}",
                            details.join("\n")
                        ))
                        .into());
                    }

                    let mutation_ids = all_reserved_entries
                        .iter()
                        .filter_map(|id| *id)
                        .collect::<std::collections::BTreeSet<_>>();
                    let mut ids = mutation_ids.iter();
                    match (ids.next(), ids.next()) {
                        (Some(mutation_id), None) => {
                            // Exactly one distinct in-flight mutation: attach.
                            let repo_names = repo_ids_and_requests
                                .iter()
                                .map(|(_id, request)| request.repo_name.as_str())
                                .collect::<Vec<_>>()
                                .join(",");
                            let mut scuba = ctx.scuba().clone();
                            scuba.add("action", "create_repos_attach");
                            scuba.add("attached_mutation_id", *mutation_id);
                            scuba.add("repo_names", repo_names);
                            scuba.log_with_msg("create_repos attached to in-flight mutation", None);
                            return Ok(ReserveOutcome::AttachedToInflight {
                                mutation_id: *mutation_id,
                            });
                        }
                        (None, _) => {
                            // Defensive: no mutation ids collected. Unreachable
                            // in practice — the all-Some guard above ensures
                            // every reserved repo has a stamped id; reachable
                            // only for an empty batch, which cannot hit the
                            // duplicate path.
                            return Err(scs_errors::invalid_request(format!(
                                "No reserved repos to attach to.\n{}",
                                details.join("\n")
                            ))
                            .into());
                        }
                        (Some(_), Some(_)) => {
                            // More than one distinct in-flight mutation.
                            return Err(scs_errors::invalid_request(format!(
                                "Repo creation request is not idempotent: the reserved repos \
                                 span multiple in-flight mutations; resolve manually.\n{}",
                                details.join("\n")
                            ))
                            .into());
                        }
                    }
                }

                Err(scs_errors::invalid_request(details.join("\n")).into())
            } else {
                Err(scs_errors::internal_error(format!(
                    "Failed to write row to git_repositories_source_of_truth. Details: {error_trace}"
                ))
                .into())
            }
        }
    }
}

fn to_repo_spec_tshirt_size(
    size_bucket: RepoSizeBucket,
) -> Result<TShirtSize, scs_errors::ServiceError> {
    match size_bucket {
        RepoSizeBucket::EXTRA_SMALL => Ok(TShirtSize::SMALL),
        RepoSizeBucket::SMALL | RepoSizeBucket::MEDIUM => Ok(TShirtSize::MEDIUM),
        RepoSizeBucket::LARGE => Ok(TShirtSize::LARGE),
        RepoSizeBucket::EXTRA_LARGE => Ok(TShirtSize::HUGE),
        _ => Err(scs_errors::internal_error(format!(
            "Unsupported RepoSizeBucket: {size_bucket:?}"
        ))
        .into()),
    }
}

// RepoSpec path helpers, Python-literal formatters, RepoIndexEntry, and
// append_to_repo_index live in `repo_spec_writer` so the admin tools and any
// other writer share one implementation. See
// `eden/mononoke/tools/repo_spec_writer/src/lib.rs`.

/// The `RepoSpec` for one new repo. The request owns identity and size; the
/// template owns everything else, verbatim, by construction: the struct tail
/// is the template, so a field added to `RepoSpec` later is template-owned
/// unless this function is changed to say otherwise.
fn make_repo_spec(
    (repo_id, request): &(RepositoryId, thrift::RepoCreationRequest),
    template: &RepoSpec,
) -> Result<RepoSpec, scs_errors::ServiceError> {
    Ok(RepoSpec {
        // Request-owned: identity and size.
        repo_id: repo_id.id(),
        repo_name: request.repo_name.clone(),
        hipster_acl: if request.custom_acl.is_some()
            || github_mirror_maintainer_oncall(request).is_some()
        {
            make_full_acl_name_from_repo_name(&request.repo_name)
        } else {
            make_top_level_acl_name_from_repo_name(&request.repo_name)
        },
        readonly: request.readonly.unwrap_or(request.github_mirror.is_some()),
        t_shirt_size: to_repo_spec_tshirt_size(request.size_bucket)?,
        // The only rule left in Rust is the additive AOSP tier on top of the
        // template's list (see repo_spec_writer::tier_list_for_repo_spec).
        tiers: tier_list_for_repo_spec(&template.tiers, &request.repo_name),
        // Request-owned when set: the storage engine, inside the template's
        // repo_config, which is otherwise copied whole.
        repo_config: match &request.storage_config {
            Some(storage_config) => template
                .repo_config
                .clone()
                .map(|repo_config| RawRepoConfig {
                    storage_config: Some(storage_config.clone()),
                    ..repo_config
                }),
            None => template.repo_config.clone(),
        },
        // Template-owned: everything else. This tail is the ownership rule.
        ..template.clone()
    })
}

async fn prepare_repo_configs_mutation_nowait(
    ctx: CoreContext,
    repos_ids_and_requests: Vec<(RepositoryId, thrift::RepoCreationRequest)>,
    configs: &MononokeConfigs,
    template_path: &str,
) -> Result<i64, scs_errors::ServiceError> {
    let configo_client = ConfigoClient::with_client(
        ctx.fb,
        make_ConfigoService_srclient!(ctx.fb)
            .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?,
    );
    let mut txn = configo_client.managed_transaction();

    // Load and validate the git RepoSpec template once, before the
    // loop and before anything is written to the transaction: a bad template
    // fails the whole call closed rather than landing a bad repo.
    let template = load_git_repo_spec_template(configs, template_path)?;

    // Write each repo's RepoSpec and derive its repo_index.cinc entry from
    // that same spec, so the index can only ever say what the spec says.
    let mut new_entries = Vec::with_capacity(repos_ids_and_requests.len());
    for (repo_id, request) in &repos_ids_and_requests {
        let repo_spec = make_repo_spec(&(*repo_id, request.clone()), &template)?;
        let entry = RepoIndexEntry::from_repo_spec(&repo_spec).map_err(|e| {
            scs_errors::internal_error(format!("Failed to derive repo_index entry: {e:#}"))
        })?;
        // Same scheme -> directory rule as the index entry's config_path, so
        // the file and the index can never name different trees.
        let dir =
            RepoSpecDir::for_scheme(repo_spec.default_commit_identity_scheme).map_err(|e| {
                scs_errors::internal_error(format!("repo {}: {e:#}", repo_spec.repo_name))
            })?;
        let file_path = make_repo_spec_file_path(&repo_spec.repo_name, dir);
        new_entries.push((repo_spec.repo_name.clone(), entry));
        txn.set_thrift_object(
            repo_spec,
            file_path,
            REPO_SPEC_THRIFT_TYPE.to_string(),
            REPO_SPEC_THRIFT_PATH.to_string(),
            None,
        );
    }

    // Update repo_index.cinc atomically in the same transaction.
    let index_path = "source/scm/mononoke/repos/repo_index.cinc".to_string();

    // Read current content — pins CAS version. Must drop handle before set_file.
    let index_str = {
        let handle = txn.get_file(index_path.clone()).await.map_err(|e| {
            scs_errors::internal_error(format!("Failed to read repo_index.cinc: {e:#}"))
        })?;
        String::from_utf8(handle.clone()).map_err(|e| {
            scs_errors::internal_error(format!("repo_index.cinc is not valid UTF-8: {e:#}"))
        })?
    }; // handle dropped — txn no longer borrowed

    let updated_index = append_to_repo_index(&index_str, &new_entries).map_err(|e| {
        scs_errors::internal_error(format!("Failed to update repo_index.cinc: {e:#}"))
    })?;
    txn.set_file(index_path, updated_index.into_bytes());

    let summary = repos_ids_and_requests
        .iter()
        .flat_map(|(repo_id, request)| format!("|{}|{}|", repo_id, request.repo_name).into_chars())
        .collect::<String>();
    let mutation = txn
        .prepare_mutation_request()
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?
        .add_author(DIFF_AUTHOR.to_string())
        .add_commit_message(
            format!(
                "[mononoke]: Create {} git repositories (automated)\n@bypass_size_limit",
                repos_ids_and_requests.len()
            ),
            summary,
        )
        .prepare_nowait()
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    Ok(mutation.id)
}

async fn update_mutation_id_by_repo_names_for_reserved_repos(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    params: &thrift::CreateReposParams,
    mutation_id: i64,
) -> Result<(), scs_errors::ServiceError> {
    let expected = params.repos.len() as u64;
    let stamped = git_source_of_truth_config
        .update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &params
                .repos
                .iter()
                .map(|request| RepositoryName(request.repo_name.clone()))
                .collect::<Vec<_>>(),
            mutation_id,
        )
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    // Every reserved row must still exist when it is stamped. A shortfall
    // means rows were lost (e.g. deleted out-of-band) after reservation:
    // fail the creation loudly before it proceeds toward landing, instead
    // of landing repos with no source-of-truth row.
    // MySQL affected_rows counts CHANGED rows; the confirm-read distinguishes a lost-ack re-stamp from genuine row loss.
    if stamped != expected
        && !all_requested_repos_stamped(&ctx, git_source_of_truth_config, params, mutation_id)
            .await?
    {
        return Err(scs_errors::internal_error(format!(
            "stamping mutation_id {mutation_id} updated {stamped} reserved row(s) but {expected} \
             were reserved; refusing to proceed, reconcile the source-of-truth table"
        ))
        .into());
    }
    Ok(())
}

async fn all_requested_repos_stamped(
    ctx: &CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    params: &thrift::CreateReposParams,
    mutation_id: i64,
) -> Result<bool, scs_errors::ServiceError> {
    // A row can flip Reserved -> Mononoke while the stamp is still retrying:
    // once the stamp has committed (even with its ack lost), a concurrent
    // duplicate request can attach to this mutation_id, and its client's poll
    // can land the mutation and flip the rows before the retry loop is done.
    // A flip proves the stamp committed, so rows in either state count.
    let stamped_names: HashSet<String> = git_source_of_truth_config
        .get_reserved_by_mutation_id(ctx, mutation_id)
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?
        .into_iter()
        .chain(
            git_source_of_truth_config
                .get_redirected_to_mononoke_by_mutation_id(ctx, mutation_id)
                .await
                .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?,
        )
        .map(|entry| entry.repo_name.0)
        .collect();
    Ok(params
        .repos
        .iter()
        .all(|request| stamped_names.contains(&request.repo_name)))
}

async fn delete_source_of_truth_for_reserved_repos(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    params: &thrift::CreateReposParams,
) -> Result<(), scs_errors::ServiceError> {
    git_source_of_truth_config
        .delete_source_of_truth_by_repo_names_for_reserved_repos(
            &ctx,
            &params
                .repos
                .iter()
                .map(|request| RepositoryName(request.repo_name.clone()))
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    Ok(())
}

async fn delete_source_of_truth_for_mutation_id(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    mutation_id: i64,
) -> Result<(), scs_errors::ServiceError> {
    git_source_of_truth_config
        .delete_source_of_truth_for_mutation_id(&ctx, &mutation_id)
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    Ok(())
}

#[cfg(fbcode_build)]
async fn create_repos_in_mononoke(
    ctx: CoreContext,
    git_source_of_truth_config: Arc<dyn GitSourceOfTruthConfig>,
    symref_store_factory: SymrefStoreFactory,
    params: &thrift::CreateReposParams,
    configs: &MononokeConfigs,
    template_path: &str,
    write_default_branch_symrefs_enabled: bool,
) -> Result<Option<i64>, scs_errors::ServiceError> {
    // ## What:
    // Create these repositories in Mononoke.
    //
    // ## How:
    // * a) Mark these repositories as reserved in the git_repositories_source_of_truth table. This
    //      will ensure that another instance of `create_repos` won't compete with us on creating these
    //      repositories
    // * b) At this stage, we have reserved the repo ids we need, so we're not in contention
    //   anymore. We have time to do slow things, such as configuring these repositories in
    //   configerator to add the to Mononoke.
    //   Use configo to make a diff adding these repositories to quick_repo_definitions using
    //   the parameters passed in to `create_repos`via thrift
    // * c) Repo creation was successful, overwrite "reserved" with "mononoke" for these
    //   repositories. At this point, the repos were created and empty, so it's OK for their Source
    //   Of Truth to be in Mononoke already, ahead of creating them in Metagit and allowing pushes.

    let (outcome, _attempts) = retry(
        |_| reserve_repos_ids(ctx.clone(), git_source_of_truth_config.as_ref(), params),
        Duration::from_millis(1_000),
    )
    .binary_exponential_backoff()
    .max_attempts(5)
    .retry_if(|_attempt, error| match error {
        // No need to retry on request error. Let's report back to the user
        scs_errors::ServiceError::Request(_) => false,
        // Internal error can indicate a db error or contention on the ids. Retry with exponential
        // backoff
        _ => true,
    })
    .await?;

    let repo_ids_and_requests = match outcome {
        ReserveOutcome::Reserved(repos) => repos,
        // A concurrent/duplicate request already reserved these repos under a
        // single in-flight mutation. Attach to it and return its token so the
        // caller can poll it to completion.
        ReserveOutcome::AttachedToInflight { mutation_id } => return Ok(Some(mutation_id)),
    };

    // Written at reservation time: the poll may run on another host that only has the mutation_id.
    let written_symref_repo_ids = match write_default_branch_symrefs(
        &ctx,
        &symref_store_factory,
        &repo_ids_and_requests,
        write_default_branch_symrefs_enabled,
    )
    .await
    {
        Ok(written) => written,
        Err(write_err) => {
            let attempted = repo_ids_and_requests
                .iter()
                .filter(|(_repo_id, request)| request.default_branch.is_some())
                .map(|(repo_id, _request)| *repo_id)
                .collect::<Vec<_>>();
            if let Err(cleanup_err) = cleanup_reserved_repos_after_failure(
                &ctx,
                git_source_of_truth_config.as_ref(),
                &symref_store_factory,
                &attempted,
                params,
            )
            .await
            {
                warn!(
                    "cleanup after create_repos symref write failure also failed; original error: {:?}",
                    write_err
                );
                return Err(cleanup_err);
            }
            return Err(write_err);
        }
    };

    // We have reserved the repo ids. Now it's time to actually create the repos, safe in the
    // knowledge that no-one will compete with us
    let reserved_repo_names = repo_ids_and_requests
        .iter()
        .map(|(_id, request)| request.repo_name.clone())
        .collect::<Vec<_>>();
    match prepare_repo_configs_mutation_nowait(
        ctx.clone(),
        repo_ids_and_requests,
        configs,
        template_path,
    )
    .await
    {
        Ok(mutation_id) => {
            let mut scuba = ctx.scuba().clone();
            scuba.add("action", "create_repos_mutation_prepared");
            scuba.add("param_repos", reserved_repo_names);
            if let Some(name) = batch_storage_name(params) {
                scuba.add("storage_config", name);
            }
            scuba.log_with_msg("create_repos prepared a repo creation mutation", None);

            retry(
                |_| {
                    update_mutation_id_by_repo_names_for_reserved_repos(
                        ctx.clone(),
                        git_source_of_truth_config.as_ref(),
                        params,
                        mutation_id,
                    )
                },
                Duration::from_millis(1_000),
            )
            .binary_exponential_backoff()
            .max_attempts(5)
            .await?;

            // Clone necessary data for the spawned task
            let poll_ctx = ctx.clone();
            let git_sot_config = git_source_of_truth_config.clone();
            let poll_symref_store_factory = symref_store_factory.clone();

            mononoke::spawn_task({
                async move {
                    // Poll interval - start with 5 seconds
                    let poll_interval = Duration::from_secs(5);

                    loop {
                        match poll_mutation_id(
                            poll_ctx.clone(),
                            git_sot_config.as_ref(),
                            &poll_symref_store_factory,
                            mutation_id,
                        )
                        .await
                        {
                            Ok(state) => match state {
                                MutationState::PREPARED
                                | MutationState::CANARYING
                                | MutationState::PREPARING
                                | MutationState::LANDING
                                | MutationState::SERVICE_CANARYING
                                | MutationState::VALIDATING => {
                                    info!("mutation in progress for mutation_id {}", mutation_id);
                                    tokio::time::sleep(poll_interval).await;
                                }
                                _ => break,
                            },
                            Err(e) => {
                                warn!(
                                    "Error polling repo creation for mutation_id: {}, error: {:?}",
                                    mutation_id, e
                                );
                                break;
                            }
                        }
                    }
                }
            });
            Ok(Some(mutation_id))
        }
        Err(e) => {
            // We failed to land the mutation, so it is safe to "release the lock" on these repo
            // ids and names, which will allow a future attempt to succeed.
            if let Err(cleanup_err) = cleanup_reserved_repos_after_failure(
                &ctx,
                git_source_of_truth_config.as_ref(),
                &symref_store_factory,
                &written_symref_repo_ids,
                params,
            )
            .await
            {
                warn!(
                    "cleanup after create_repos mutation prepare failure also failed; original error: {:?}",
                    e
                );
                return Err(cleanup_err);
            }
            Err(e)
        }
    }
}

#[cfg(not(fbcode_build))]
async fn create_repos_in_mononoke(
    _ctx: CoreContext,
    _git_source_of_truth_config: Arc<dyn GitSourceOfTruthConfig>,
    _symref_store_factory: SymrefStoreFactory,
    _params: &thrift::CreateReposParams,
    _configs: &MononokeConfigs,
    _template_path: &str,
    _write_default_branch_symrefs_enabled: bool,
) -> Result<Option<i64>, scs_errors::ServiceError> {
    println!("No access to configo in oss build");
    Ok(None)
}

impl SourceControlServiceImpl {
    fn symref_store_factory(&self) -> SymrefStoreFactory {
        SymrefStoreFactory {
            fb: self.fb,
            configs: self.configs.clone(),
            mysql_options: self.mysql_options.clone(),
            builders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn create_repos(
        &self,
        ctx: CoreContext,
        params: thrift::CreateReposParams,
    ) -> Result<thrift::CreateReposToken, scs_errors::ServiceError> {
        // Must precede `ensure_acls_allow_repo_creation`, which fans out one
        // Hipster/Oncall RPC per requested repo with an unbounded `try_join_all`:
        // a cap applied afterwards would already have paid the cost it bounds.
        //
        // Advisory unless the enforce knob is on. `create_repos` has always
        // accepted any batch, including empty ones, and both the bulk-import
        // pipeline and `scmadmin repo tasks` are unattended, so a rejection
        // that arrives without a measured log-only phase fails a partner run on
        // a number we guessed (`config_rollout_safety.md`, S578742).
        let count = params.repos.len();
        let tier = batch_size_tier_for_caller(&ctx, self.acl_provider.as_ref()).await;
        if let Err(error) = check_batch_size(count, tier.max_batch_size()) {
            if justknobs::eval(ENFORCE_BATCH_SIZE_JK, None, Some(tier.name())) {
                return Err(error);
            }
            warn!(
                count,
                max_batch_size = tier.max_batch_size(),
                tier = tier.name(),
                "create_repos batch is outside the cap for this tier; admitting it because \
                 enforcement is off"
            );
        }

        let write_default_branch_symrefs_enabled = validate_default_branches(&params.repos)?;
        let storage = validate_storage_configs(&params.repos, &self.configs)?;
        let params = with_batch_storage(params, &storage);

        ensure_acls_allow_repo_creation(ctx.clone(), &params.repos, self.acl_provider.as_ref())
            .await?;
        update_repos_acls(ctx.clone(), &params).await?;
        let mutation_id = create_repos_in_mononoke(
            ctx,
            self.git_source_of_truth_config.clone(),
            self.symref_store_factory(),
            &params,
            &self.configs,
            storage.repo_spec_template_path(),
            write_default_branch_symrefs_enabled,
        )
        .await?;

        Ok(thrift::CreateReposToken {
            mutation_id,
            ..Default::default()
        })
    }

    #[cfg(fbcode_build)]
    pub(crate) async fn create_repos_poll(
        &self,
        ctx: CoreContext,
        token: thrift::CreateReposToken,
    ) -> Result<thrift::CreateReposPollResponse, scs_errors::ServiceError> {
        if token.mutation_id.is_none() {
            return Err(scs_errors::invalid_request("mutation_id is not set".to_string()).into());
        }

        let mutation_id = token.mutation_id.unwrap();
        let symref_store_factory = self.symref_store_factory();
        let mutation_state;
        let status = match poll_mutation_id(
            ctx,
            self.git_source_of_truth_config.as_ref(),
            &symref_store_factory,
            mutation_id,
        )
        .await
        {
            Ok(state) => {
                mutation_state = state;
                match state {
                    MutationState::LANDED => thrift::CreateReposStatus::SUCCESS,
                    MutationState::FAILED => thrift::CreateReposStatus::FAILED,
                    MutationState::ABORTED => thrift::CreateReposStatus::ABORTED,
                    MutationState::PREPARED
                    | MutationState::CANARYING
                    | MutationState::PREPARING
                    | MutationState::LANDING
                    | MutationState::SERVICE_CANARYING
                    | MutationState::VALIDATING => thrift::CreateReposStatus::IN_PROGRESS,
                    _ => {
                        return Err(scs_errors::internal_error(format!(
                            "Unexpected Configo mutation state: {state}"
                        ))
                        .into());
                    }
                }
            }
            Err(err) => return Err(err),
        };

        let message = Some(format!("Mutation state: {mutation_state}"));
        Ok(thrift::CreateReposPollResponse {
            result: Some(thrift::CreateReposResponse {
                status,
                message,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

#[cfg(fbcode_build)]
async fn poll_mutation_id(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    symref_store_provider: &dyn SymrefStoreProvider,
    mutation_id: i64,
) -> Result<MutationState, scs_errors::ServiceError> {
    match poll_mutation_id_impl(
        ctx.clone(),
        git_source_of_truth_config,
        symref_store_provider,
        mutation_id,
        0,
    )
    .await
    {
        Ok(state) => Ok(state),
        Err(e) => match e {
            scs_errors::ServiceError::Poll(_) => {
                poll_mutation_id_impl(
                    ctx.clone(),
                    git_source_of_truth_config,
                    symref_store_provider,
                    mutation_id,
                    1,
                )
                .await
            }
            _ => Err(e),
        },
    }
}

async fn handle_landed_state(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    mutation_id: i64,
) -> Result<(), scs_errors::ServiceError> {
    flip_landed_mutation_to_mononoke(&ctx, git_source_of_truth_config, mutation_id)
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;
    Ok(())
}

async fn handle_prepared_state(
    ctx: CoreContext,
    configo_client: ConfigoServiceClient,
    mutation_id: i64,
    is_signed: bool,
    retry_count: i64,
    error_message: String,
) -> Result<(), scs_errors::ServiceError> {
    if !is_signed {
        if let Err(e) = initiate_land_for_mutation(ctx, configo_client, mutation_id).await {
            if retry_count == 0 {
                return Err(scs_errors::poll_error(format!(
                    "Configo mutation error: {error_message}"
                ))
                .into());
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

async fn handle_mutation_state(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    symref_store_provider: &dyn SymrefStoreProvider,
    configo_client: ConfigoServiceClient,
    state: MutationState,
    mutation_id: i64,
    is_signed: bool,
    retry_count: i64,
    error_message: String,
) -> Result<(), scs_errors::ServiceError> {
    match state {
        MutationState::LANDED => {
            handle_landed_state(ctx, git_source_of_truth_config, mutation_id).await?;
        }
        MutationState::FAILED | MutationState::ABORTED => {
            cleanup_repos(
                ctx,
                git_source_of_truth_config,
                symref_store_provider,
                mutation_id,
            )
            .await?;
        }
        MutationState::PREPARED => {
            handle_prepared_state(
                ctx,
                configo_client,
                mutation_id,
                is_signed,
                retry_count,
                error_message,
            )
            .await?;
        }
        _ => (),
    }
    Ok(())
}

async fn poll_mutation_id_impl(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    symref_store_provider: &dyn SymrefStoreProvider,
    mutation_id: i64,
    retry_count: i64,
) -> std::result::Result<MutationState, scs_errors::ServiceError> {
    let configo_client = make_ConfigoService_srclient!(ctx.fb)
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;

    let resp = configo_client
        .status(&mutation_id)
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;

    if resp.error {
        cleanup_repos(
            ctx.clone(),
            git_source_of_truth_config,
            symref_store_provider,
            mutation_id,
        )
        .await?;
        return Err(scs_errors::internal_error(format!(
            "Configo mutation error: {}",
            resp.errorMessage
        ))
        .into());
    }

    let mutation = resp.mutation.ok_or_else(|| {
        warn!("mutation state is not present {}", mutation_id);
        scs_errors::internal_error(format!("Mutation state not available for {mutation_id}"))
    })?;

    if mutation.stateInfo.isError {
        info!("cleaning up, mutation state info has error {}", mutation_id);
        cleanup_repos(
            ctx.clone(),
            git_source_of_truth_config,
            symref_store_provider,
            mutation_id,
        )
        .await?;
        return Err(scs_errors::internal_error(format!(
            "Configo mutation error: {}",
            mutation.stateInfo.errorMessage
        ))
        .into());
    }

    let state = mutation.stateInfo.state;
    handle_mutation_state(
        ctx,
        git_source_of_truth_config,
        symref_store_provider,
        configo_client,
        state,
        mutation_id,
        mutation.stateInfo.isSigned,
        retry_count,
        mutation.stateInfo.errorMessage,
    )
    .await?;

    Ok(state)
}

async fn delete_head_symrefs_for_mutation_id(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    symref_store_provider: &dyn SymrefStoreProvider,
    mutation_id: i64,
) -> Result<(), scs_errors::ServiceError> {
    let reserved_repos = git_source_of_truth_config
        .get_reserved_by_mutation_id(&ctx, mutation_id)
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?
        .into_iter()
        .map(|entry| (entry.repo_id, entry.storage_config))
        .collect::<Vec<_>>();
    delete_head_symrefs(&ctx, symref_store_provider, &reserved_repos).await
}

/// Two concurrent pollers can both run this; a stalled one could delete a fresh HEAD row
/// after id reuse plus re-reserve+rewrite. Not atomically fixable across the two DBs; accepted.
async fn cleanup_repos(
    ctx: CoreContext,
    git_source_of_truth_config: &dyn GitSourceOfTruthConfig,
    symref_store_provider: &dyn SymrefStoreProvider,
    mutation_id: i64,
) -> Result<(), scs_errors::ServiceError> {
    // Symref deletes go before the SoT deletes so a failure leaves the rows Reserved and retryable.
    let (_result, _attempts) = retry(
        |_| {
            delete_head_symrefs_for_mutation_id(
                ctx.clone(),
                git_source_of_truth_config,
                symref_store_provider,
                mutation_id,
            )
        },
        Duration::from_millis(1_000),
    )
    .binary_exponential_backoff()
    .max_attempts(5)
    .await?;
    let (_result, _attempts) = retry(
        |_| {
            delete_source_of_truth_for_mutation_id(
                ctx.clone(),
                git_source_of_truth_config,
                mutation_id,
            )
        },
        Duration::from_millis(1_000),
    )
    .binary_exponential_backoff()
    .max_attempts(5)
    .await?;
    Ok(())
}

#[cfg(fbcode_build)]
pub(crate) async fn initiate_land_for_mutation(
    ctx: CoreContext,
    configo_client: ConfigoServiceClient,
    mutation_id: i64,
) -> Result<(), scs_errors::ServiceError> {
    let mutation = Mutation::new(ctx.fb, configo_client, mutation_id);

    let mutation_id = mutation
        .land_nowait()
        .await
        .map_err(|e| scs_errors::internal_error(format!("{e:#}")))?;

    info!("initiated land for mutation id  {}", mutation_id.id);
    Ok(())
}

#[cfg(all(fbcode_build, test))]
mod attach_tests;
#[cfg(all(fbcode_build, test))]
mod symref_tests;
#[cfg(test)]
mod tests;
