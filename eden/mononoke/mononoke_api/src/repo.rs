/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
#[cfg(fbcode_build)]
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ::commit_cloud::CommitCloud;
use ::commit_graph::ArcCommitGraph;
use ::commit_graph::CommitGraph;
use ::commit_graph::CommitGraphArc;
use ::commit_graph::CommitGraphRef;
use ::commit_graph::CommitGraphWriter;
#[cfg(fbcode_build)]
use MononokeApiStats_ods3::Instrument_MononokeApiStats;
#[cfg(fbcode_build)]
use MononokeApiStats_ods3_types::MononokeApiEvent;
#[cfg(fbcode_build)]
use MononokeApiStats_ods3_types::MononokeApiStats;
use acl_regions::AclRegions;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use blobrepo_hg::BlobRepoHg;
use blobstore::Loadable;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_git_mapping::BonsaiGitMappingRef;
use bonsai_git_mapping::BonsaisOrGitShas;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingRef;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_p4_mapping::BonsaiP4MappingRef;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMappingRef;
use bonsai_tag_mapping::BonsaiTagMapping;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkName;
use bookmarks::BookmarkPagination;
use bookmarks::BookmarkPrefix;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Bookmarks;
use bookmarks::BookmarksRef;
pub use bookmarks::Freshness as BookmarkFreshness;
use bookmarks::Freshness;
use bookmarks_cache::BookmarksCache;
use bookmarks_cache::BookmarksCacheRef;
use bulk_derivation::BulkDerivation;
use bundle_uri::GitBundleUri;
use bytes::Bytes;
use changeset_info::ChangesetInfo;
use commit_rate_limit_config::CommitRateLimit;
use context::CoreContext;
use context::PerfCounterType;
use cross_repo_sync::CandidateSelectionHint;
use cross_repo_sync::CommitSyncContext;
use cross_repo_sync::CommitSyncData;
use cross_repo_sync::CommitSyncRepos;
use cross_repo_sync::RepoProvider;
use cross_repo_sync::Target;
use cross_repo_sync::get_all_repo_submodule_deps;
use cross_repo_sync::get_all_submodule_deps_from_repo_pair;
use cross_repo_sync::get_small_and_large_repos;
use cross_repo_sync::sync_commit;
use dag_types::Location;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivableType;
use enabled_derived_data_types::EnabledDerivedDataTypes;
use ephemeral_blobstore::ArcRepoEphemeralStore;
use ephemeral_blobstore::Bubble;
use ephemeral_blobstore::BubbleId;
use ephemeral_blobstore::RepoEphemeralStore;
use ephemeral_blobstore::RepoEphemeralStoreArc;
use ephemeral_blobstore::RepoEphemeralStoreRef;
use ephemeral_blobstore::StorageLocation;
use filenodes::Filenodes;
use filestore::Alias;
use filestore::FetchKey;
use filestore::FilestoreConfig;
use filestore::FilestoreConfigRef;
pub use filestore::StoreRequest;
use futures::Future;
use futures::TryFutureExt;
use futures::future;
use futures::stream;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_lazy_shared::LazyShared;
use futures_watchdog::WatchdogExt;
use git_ref_content_mapping::GitRefContentMapping;
use git_source_of_truth::GitSourceOfTruthConfig;
use git_symbolic_refs::GitSymbolicRefs;
use git_types::CgdmChangesetDivider;
use git_types::MappedGitCommitId;
use hook_manager::manager::HookManager;
use hook_manager::manager::HookManagerArc;
use itertools::Itertools;
use live_commit_sync_config::LiveCommitSyncConfig;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_mutation::HgMutationStore;
use mercurial_types::Globalrev;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use metaconfig_types::RepoConfigSource;
use metaconfig_types::RepoConfigSourceRef;
use mononoke_repos::MononokeRepos;
use mononoke_types::ContentId;
use mononoke_types::ContentManifestId;
use mononoke_types::P4ChangelistId;
use mononoke_types::RepositoryId;
use mononoke_types::Svnrev;
use mononoke_types::Timestamp;
use mononoke_types::hash::Blake3;
use mononoke_types::hash::GitSha1;
use mononoke_types::hash::Sha1;
use mononoke_types::hash::Sha256;
use mutable_blobstore::MutableRepoBlobstore;
use mutable_counters::MutableCounters;
use mutable_renames::ArcMutableRenames;
use mutable_renames::MutableRenames;
use mutable_renames::MutableRenamesArc;
use phases::Phases;
use phases::PhasesRef;
use pushrebase_mutation_mapping::PushrebaseMutationMapping;
use repo_authorization::AuthorizationContext;
use repo_blobstore::ArcRepoBlobstore;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_bookmark_attrs::RepoBookmarkAttrs;
use repo_cross_repo::RepoCrossRepo;
use repo_cross_repo::RepoCrossRepoRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_event_publisher::RepoEventPublisher;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use repo_lock::RepoLock;
use repo_permission_checker::RepoPermissionChecker;
use repo_permission_checker::RepoPermissionCheckerRef;
use repo_sparse_profiles::ArcRepoSparseProfiles;
use repo_sparse_profiles::RepoSparseProfiles;
use repo_sparse_profiles::RepoSparseProfilesArc;
use repo_stats_logger::RepoStatsLogger;
use restricted_paths::RestrictedPaths;
use restricted_paths::RestrictedPathsArc;
use sql_commit_graph_storage::CommitGraphBulkFetcher;
use sql_query_config::SqlQueryConfig;
use stats::prelude::*;
use streaming_clone::StreamingClone;
use synced_commit_mapping::ArcSyncedCommitMapping;
use tracing::debug;
use tracing::error;
use unbundle::PushRedirector;
use unbundle::PushRedirectorArgs;
use wireproto_handler::PushRedirectorBase;
use wireproto_handler::RepoHandlerBase;
use wireproto_handler::RepoHandlerBaseRef;

use crate::changeset::ChangesetContext;
use crate::errors::MononokeError;
use crate::file::FileContext;
use crate::file::FileId;
use crate::specifiers::ChangesetId;
use crate::specifiers::ChangesetPrefixSpecifier;
use crate::specifiers::ChangesetSpecifier;
use crate::specifiers::ChangesetSpecifierPrefixResolution;
use crate::specifiers::HgChangesetId;
use crate::tree::TreeContext;
use crate::xrepo::CandidateSelectionHintArgs;

pub mod check_rebase_conflicts;
pub mod commit_cloud;
pub mod commit_graph;
pub mod create_bookmark;
pub mod create_changeset;
pub mod delete_bookmark;
pub mod fold_commits;
pub mod git;
pub mod land_stack;
pub mod move_bookmark;
pub mod rebase_stack;
pub mod update_submodule_expansion;

pub use git::upload_non_blob_git_object;

#[cfg(fbcode_build)]
static API_STATS_INSTRUMENT: LazyLock<Instrument_MononokeApiStats> =
    LazyLock::new(Instrument_MononokeApiStats::new);

define_stats! {
    prefix = "mononoke.api";
    staleness: dynamic_singleton_counter(
        "staleness.secs.{}.{}",
        (repoid: ::mononoke_types::RepositoryId, bookmark: String)
    ),
}

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    pub repo_blobstore: RepoBlobstore,

    #[facet]
    pub mutable_repo_blobstore: MutableRepoBlobstore,

    #[facet]
    pub repo_bookmark_attrs: RepoBookmarkAttrs,

    #[facet]
    pub repo_derived_data: RepoDerivedData,

    #[facet]
    pub repo_identity: RepoIdentity,

    #[facet]
    pub bonsai_tag_mapping: dyn BonsaiTagMapping,

    #[facet]
    pub bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    pub git_ref_content_mapping: dyn GitRefContentMapping,

    #[facet]
    pub git_bundle_uri: dyn GitBundleUri,

    #[facet]
    pub bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    pub repo_event_publisher: dyn RepoEventPublisher,

    #[facet]
    pub bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

    #[facet]
    pub bonsai_p4_mapping: dyn BonsaiP4Mapping,

    #[facet]
    pub bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    pub bookmark_update_log: dyn BookmarkUpdateLog,

    #[facet]
    pub bookmarks: dyn Bookmarks,

    #[facet]
    pub phases: dyn Phases,

    #[facet]
    pub pushrebase_mutation_mapping: dyn PushrebaseMutationMapping,

    #[facet]
    pub hg_mutation_store: dyn HgMutationStore,

    #[facet]
    pub mutable_counters: dyn MutableCounters,

    #[facet]
    pub repo_permission_checker: dyn RepoPermissionChecker,

    #[facet]
    pub repo_lock: dyn RepoLock,

    #[facet]
    pub repo_config: RepoConfig,

    #[facet]
    pub repo_config_source: RepoConfigSource,

    #[facet]
    pub repo_ephemeral_store: RepoEphemeralStore,

    #[facet]
    pub mutable_renames: MutableRenames,

    #[facet]
    pub repo_cross_repo: RepoCrossRepo,

    #[facet]
    pub acl_regions: dyn AclRegions,

    #[facet]
    pub repo_sparse_profiles: RepoSparseProfiles,

    #[facet]
    pub streaming_clone: StreamingClone,

    #[facet]
    pub commit_graph: CommitGraph,

    #[facet]
    pub commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    pub git_symbolic_refs: dyn GitSymbolicRefs,

    #[facet]
    pub git_source_of_truth_config: dyn GitSourceOfTruthConfig,

    #[facet]
    pub enabled_derived_data_types: dyn EnabledDerivedDataTypes,

    #[facet]
    pub filenodes: dyn Filenodes,

    #[facet]
    pub commit_cloud: CommitCloud,

    #[facet]
    pub sql_query_config: SqlQueryConfig,

    #[facet]
    pub warm_bookmarks_cache: dyn BookmarksCache,

    #[facet]
    pub hook_manager: HookManager,

    #[facet]
    pub repo_handler_base: RepoHandlerBase,

    #[facet]
    pub filestore_config: FilestoreConfig,

    #[facet]
    pub repo_stats_logger: RepoStatsLogger,

    #[facet]
    pub commit_graph_bulk_fetcher: CommitGraphBulkFetcher,

    #[facet]
    restricted_paths: RestrictedPaths,

    #[facet]
    pub commit_rate_limit: CommitRateLimit,

    #[facet]
    pub cgdm_changeset_divider: dyn CgdmChangesetDivider,
}

pub trait MononokeRepo = RepoLike + RepoWithBubble + Clone + 'static;

#[derive(Clone)]
pub struct RepoContext<R> {
    ctx: CoreContext,
    authz: Arc<AuthorizationContext>,
    repo: Arc<R>,
    /// Lazily resolved push redirector. Only relevant on the write path
    /// (pushrebase, bookmark moves) and for reporting in `RepoInfo`. Resolving
    /// it eagerly would force every `RepoContext` consumer to satisfy
    /// `RepoCrossRepoRef + RepoHandlerBaseRef` bounds, so we defer it to first
    /// use via `push_redirector()`.
    push_redirector: LazyShared<Result<Option<Arc<PushRedirector<R>>>, MononokeError>>,
    repos: Arc<MononokeRepos<R>>,
    /// Identity type that granted repo-level read access (from ACL check).
    repo_acl_deciding_identity_type: Option<String>,
    /// Identity types that granted path-level read access (from region ACL checks).
    path_acl_deciding_identity_types: Arc<Mutex<Vec<String>>>,
}

impl<R: RepoIdentityRef> fmt::Debug for RepoContext<R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "RepoContext(repo={:?})",
            self.repo.repo_identity().name()
        )
    }
}

pub struct RepoContextBuilder<R> {
    ctx: CoreContext,
    authz: Option<AuthorizationContext>,
    repo: Arc<R>,
    bubble_id: Option<BubbleId>,
    repos: Arc<MononokeRepos<R>>,
}

pub async fn push_redirector_enabled<R: MononokeRepo>(
    ctx: &CoreContext,
    repo: Arc<R>,
) -> Result<bool> {
    let live_commit_sync_config = repo.repo_cross_repo().live_commit_sync_config();
    live_commit_sync_config
        .push_redirector_enabled_for_public(ctx, repo.repo_identity().id())
        .await
}

async fn maybe_push_redirector<'a, R: MononokeRepo>(
    ctx: &'a CoreContext,
    repo: &'a Arc<R>,
    repos: &'a MononokeRepos<R>,
) -> Result<Option<PushRedirector<R>>, MononokeError> {
    let base = match repo.repo_handler_base().maybe_push_redirector_base.as_ref() {
        None => return Ok(None),
        Some(base) => base,
    };
    let live_commit_sync_config = repo.repo_cross_repo().live_commit_sync_config();
    let enabled = live_commit_sync_config
        .push_redirector_enabled_for_public(ctx, repo.repo_identity().id())
        .await?;

    if enabled {
        let repo_provider: RepoProvider<'a, R> = Arc::new(move |repo_id| {
            Box::pin({
                async move {
                    let repo = repos
                        .get_by_id(repo_id.id())
                        .ok_or_else(|| anyhow!("Submodule dependency repo with id {repo_id} not available through RepoContext"))?;
                    Ok(repo)
                }
            })
        });

        let submodule_deps = get_all_repo_submodule_deps(ctx, repo.clone(), repo_provider).await?;

        let large_repo_id = base.common_commit_sync_config.large_repo_id;
        let large_repo = repos
            .get_by_id(large_repo_id.id())
            .ok_or_else(|| MononokeError::LargeRepoNotFound(format!("{large_repo_id}")))?;
        Ok(Some(
            PushRedirectorArgs::new(
                large_repo,
                repo.clone(),
                base.synced_commit_mapping.clone(),
                base.target_repo_dbs.clone(),
            )
            .into_push_redirector(ctx, live_commit_sync_config.clone(), submodule_deps)
            .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?,
        ))
    } else {
        Ok(None)
    }
}

impl<R> RepoContextBuilder<R> {
    pub fn with_authorization_context(mut self, authz: AuthorizationContext) -> Self {
        self.authz = Some(authz);
        self
    }
}

impl<R> RepoContextBuilder<R> {
    pub async fn new(
        ctx: CoreContext,
        repo: Arc<R>,
        repos: Arc<MononokeRepos<R>>,
    ) -> Result<Self, MononokeError> {
        Ok(RepoContextBuilder {
            ctx,
            authz: None,
            repo,
            bubble_id: None,
            repos,
        })
    }
}

impl<R: RepoEphemeralStoreRef> RepoContextBuilder<R> {
    pub async fn with_bubble<F, Fut>(mut self, bubble_fetcher: F) -> Result<Self, MononokeError>
    where
        F: FnOnce(RepoEphemeralStore) -> Fut,
        Fut: Future<Output = anyhow::Result<Option<BubbleId>>>,
    {
        self.bubble_id = bubble_fetcher(self.repo.repo_ephemeral_store().clone()).await?;
        Ok(self)
    }
}

impl<R> RepoContextBuilder<R>
where
    R: RepoPermissionCheckerRef
        + RepoIdentityRef
        + RepoEphemeralStoreRef
        + RepoWithBubble
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub async fn build(self) -> Result<RepoContext<R>, MononokeError> {
        let authz = Arc::new(
            self.authz
                .clone()
                .unwrap_or_else(|| AuthorizationContext::new(&self.ctx)),
        );
        RepoContext::new(self.ctx, authz, self.repo, self.bubble_id, self.repos).await
    }
}

/// Defines behavuiour of xrepo_commit_lookup when there's no mapping for queries commit just yet.
#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum XRepoLookupSyncBehaviour {
    // Initiates sync and returns the sync result
    SyncIfAbsent,
    // Returns None
    NeverSync,
}

/// Defines behavuiour of xrepo_commit_lookup when there's no exact mapping but only working copy equivalence
#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum XRepoLookupExactBehaviour {
    // Returns result only when there's exact mapping
    OnlyExactMapping,
    // Returns result also when there's working copy equivalent match
    WorkingCopyEquivalence,
}

pub trait MonitoredRepo = BookmarksCacheRef
    + BookmarksRef
    + RepoBlobstoreRef
    + RepoIdentityRef
    + RepoConfigRef
    + CommitGraphRef;

pub async fn report_monitoring_stats(
    ctx: &CoreContext,
    repo: &impl MonitoredRepo,
) -> Result<(), MononokeError> {
    match repo
        .repo_config()
        .source_control_service_monitoring
        .as_ref()
    {
        None => {}
        Some(monitoring_config) => {
            for bookmark in monitoring_config.bookmarks_to_report_age.iter() {
                report_bookmark_age_difference(ctx, repo, bookmark).await?;
            }
        }
    }

    Ok(())
}

fn report_bookmark_missing_from_cache(repo: &impl RepoIdentityRef, bookmark: &BookmarkKey) {
    error!(
        "Monitored bookmark does not exist in the cache: {}, repo: {}",
        bookmark,
        repo.repo_identity().name()
    );

    #[cfg(fbcode_build)]
    API_STATS_INSTRUMENT.observe(MononokeApiStats {
        repo: Some(repo.repo_identity().name().to_string()),
        repoid: Some(repo.repo_identity().id().id()),
        bookmark: Some(bookmark.to_string()),
        event: Some(MononokeApiEvent::BookmarkNotInCache),
        event_count: Some(1.0),
        ..Default::default()
    });
}

#[cfg_attr(
    not(fbcode_build),
    expect(
        unused_variables,
        reason = "Only internal builds report to the API stats instrument"
    )
)]
fn report_bookmark_missing_from_repo(repo: &impl RepoIdentityRef, bookmark: &BookmarkKey) {
    error!(
        "Monitored bookmark does not exist in the repo: {}",
        bookmark
    );

    #[cfg(fbcode_build)]
    API_STATS_INSTRUMENT.observe(MononokeApiStats {
        repo: Some(repo.repo_identity().name().to_string()),
        repoid: Some(repo.repo_identity().id().id()),
        bookmark: Some(bookmark.to_string()),
        event: Some(MononokeApiEvent::BookmarkNotInRepo),
        event_count: Some(1.0),
        ..Default::default()
    });
}

fn report_bookmark_staleness(
    ctx: &CoreContext,
    repo: &impl RepoIdentityRef,
    bookmark: &BookmarkKey,
    staleness: i64,
    blobrepo_bcs_id: ChangesetId,
    service_bcs_id: ChangesetId,
) {
    // Don't log if staleness is less than 10 to make output less spammy
    if staleness >= 10 {
        debug!(
            "Reporting staleness of {bookmark} to be {staleness}s: latest value is {blobrepo_bcs_id}, cache points to {service_bcs_id}",
        );
    }

    STATS::staleness.set_value(
        ctx.fb,
        staleness,
        (repo.repo_identity().id(), bookmark.to_string()),
    );

    #[cfg(fbcode_build)]
    API_STATS_INSTRUMENT.observe(MononokeApiStats {
        repo: Some(repo.repo_identity().name().to_string()),
        repoid: Some(repo.repo_identity().id().id()),
        bookmark: Some(bookmark.to_string()),
        event: Some(MononokeApiEvent::BookmarkStale),
        event_count: Some(1.0),
        ..Default::default()
    });
}

async fn report_bookmark_age_difference(
    ctx: &CoreContext,
    repo: &impl MonitoredRepo,
    bookmark: &BookmarkKey,
) -> Result<(), MononokeError> {
    let maybe_bcs_id_from_service = repo.bookmarks_cache().get(ctx, bookmark).await?;
    let maybe_bcs_id_from_blobrepo = repo
        .bookmarks()
        .get(ctx.clone(), bookmark, bookmarks::Freshness::MostRecent)
        .await?;

    if maybe_bcs_id_from_blobrepo.is_none() {
        report_bookmark_missing_from_repo(repo, bookmark);
    }

    if maybe_bcs_id_from_service.is_none() {
        report_bookmark_missing_from_cache(repo, bookmark);
    }

    if let (Some(service_bcs_id), Some(blobrepo_bcs_id)) =
        (maybe_bcs_id_from_service, maybe_bcs_id_from_blobrepo)
    {
        // We report the difference between current time (i.e. SystemTime::now())
        // and timestamp of the first child of bookmark value from cache (see graph below)
        //
        //       O <- bookmark value from blobrepo
        //       |
        //      ...
        //       |
        //       O <- first child of bookmark value from cache.
        //       |
        //       O <- bookmark value from cache, it's outdated
        //
        // This way of reporting shows for how long the oldest commit not in cache hasn't been
        // imported, and it should work correctly both for high and low commit rates.

        let difference = if blobrepo_bcs_id == service_bcs_id {
            0
        } else {
            let limit = 100;
            let maybe_child =
                try_find_child(ctx, repo, service_bcs_id, blobrepo_bcs_id, limit).await?;

            // If we can't find a child of a bookmark value from cache, then it might mean
            // that either cache is too far behind or there was a non-forward bookmark move.
            // Either way, we can't really do much about it here, so let's just find difference
            // between current timestamp and bookmark value from cache.
            let compare_bcs_id = maybe_child.unwrap_or(service_bcs_id);

            let compare_timestamp = compare_bcs_id
                .load(ctx, repo.repo_blobstore())
                .await?
                .author_date()
                .timestamp_secs();

            let current_timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(Error::from)?;
            let current_timestamp = current_timestamp.as_secs() as i64;
            current_timestamp - compare_timestamp
        };
        report_bookmark_staleness(
            ctx,
            repo,
            bookmark,
            difference,
            blobrepo_bcs_id,
            service_bcs_id,
        );
    }

    Ok(())
}

/// Try to find a changeset that's ancestor of `descendant` and direct child of
/// `ancestor`. Returns None if this commit doesn't exist (for example if `ancestor` is not
/// actually an ancestor of `descendant`) or if child is too far away from descendant.
async fn try_find_child(
    ctx: &CoreContext,
    repo: &impl CommitGraphRef,
    ancestor: ChangesetId,
    descendant: ChangesetId,
    limit: u64,
) -> Result<Option<ChangesetId>, Error> {
    // This is a generation number beyond which we don't need to traverse
    let min_gen_num = repo
        .commit_graph()
        .changeset_generation(ctx, ancestor)
        .await?;

    let mut ancestors = repo
        .commit_graph()
        .ancestors_difference_stream(ctx, vec![descendant], vec![])
        .await?;

    let mut traversed = 0;
    while let Some(cs_id) = ancestors.next().await {
        traversed += 1;
        if traversed > limit {
            return Ok(None);
        }

        let cs_id = cs_id?;
        let parents = repo.commit_graph().changeset_parents(ctx, cs_id).await?;

        if parents.contains(&ancestor) {
            return Ok(Some(cs_id));
        } else {
            let gen_num = repo.commit_graph().changeset_generation(ctx, cs_id).await?;
            if gen_num < min_gen_num {
                return Ok(None);
            }
        }
    }

    Ok(None)
}

/// Trait for repo objects that can be wrapped in a bubble.
pub trait RepoWithBubble {
    /// Construct a new Repo based on an existing one with a bubble opened.
    fn with_bubble(&self, bubble: Bubble) -> Self;
}

impl RepoWithBubble for Repo {
    fn with_bubble(&self, bubble: Bubble) -> Self {
        let repo_blobstore = Arc::new(bubble.wrap_repo_blobstore(self.repo_blobstore().clone()));
        let commit_graph = Arc::new(bubble.repo_commit_graph(self));
        let commit_graph_writer = bubble.repo_commit_graph_writer(self);
        let repo_derived_data = Arc::new(self.repo_derived_data().for_bubble(bubble.clone()));
        let bonsai_hg_mapping = bubble.bonsai_hg_mapping(
            self.repo_identity().id(),
            self.repo_identity().name(),
            self.bonsai_hg_mapping.clone(),
        );
        // RestrictedPaths holds its own ArcRepoDerivedData snapshot — rewrap it
        // so AclManifest derivation for bubble changesets routes through the
        // bubble-aware derived-data manager (and not DDS, which doesn't know
        // about ephemeral commits).
        let restricted_paths = Arc::new(self.restricted_paths.for_bubble(bubble));

        Self {
            repo_blobstore,
            commit_graph,
            commit_graph_writer,
            repo_derived_data,
            restricted_paths,
            bonsai_hg_mapping,
            ..self.clone()
        }
    }
}

#[derive(Default)]
pub struct Stack {
    pub draft: Vec<ChangesetId>,
    pub public: Vec<ChangesetId>,
    pub leftover_heads: Vec<ChangesetId>,
}

pub struct BookmarkInfo<R> {
    pub warm_changeset: ChangesetContext<R>,
    pub fresh_changeset: ChangesetContext<R>,
    pub last_update_timestamp: Timestamp,
}

/// A context object representing a query to a particular repo.
impl<R> RepoContext<R> {
    /// The context for this query.
    pub fn ctx(&self) -> &CoreContext {
        &self.ctx
    }

    /// The authorization context of the request.
    pub fn authorization_context(&self) -> &AuthorizationContext {
        &self.authz
    }

    pub fn repo(&self) -> &R {
        self.repo.as_ref()
    }

    pub fn repo_arc(&self) -> Arc<R> {
        self.repo.clone()
    }

    /// The identity type that granted repo-level read access.
    pub fn repo_acl_deciding_identity_type(&self) -> Option<&str> {
        self.repo_acl_deciding_identity_type.as_deref()
    }

    /// The identity types that granted path-level read access.
    pub fn path_acl_deciding_identity_types(&self) -> Vec<String> {
        self.path_acl_deciding_identity_types
            .lock()
            .expect("lock poisoned")
            .clone()
    }

    /// Record an identity type that granted path-level read access.
    pub fn record_path_acl_deciding_identity_type(&self, id_type: String) {
        self.path_acl_deciding_identity_types
            .lock()
            .expect("lock poisoned")
            .push(id_type);
    }
}

impl<R: RepoIdentityRef> RepoContext<R> {
    /// The name of the underlying repo.
    pub fn name(&self) -> &str {
        self.repo.repo_identity().name()
    }

    /// The internal id of the repo. Used for comparing the repo objects with each other.
    pub fn repoid(&self) -> RepositoryId {
        self.repo.repo_identity().id()
    }
}

impl<R: RepoCrossRepoRef> RepoContext<R> {
    /// `LiveCommitSyncConfig` instance to query current state of sync configs.
    pub fn live_commit_sync_config(&self) -> Arc<dyn LiveCommitSyncConfig> {
        self.repo
            .repo_cross_repo()
            .live_commit_sync_config()
            .clone()
    }

    /// The commit sync mapping for the referenced repository
    pub fn synced_commit_mapping(&self) -> &ArcSyncedCommitMapping {
        self.repo.repo_cross_repo().synced_commit_mapping()
    }
}

impl<R: RepoEphemeralStoreArc> RepoContext<R> {
    /// The ephemeral store for the referenced repository
    pub fn repo_ephemeral_store_arc(&self) -> ArcRepoEphemeralStore {
        self.repo.repo_ephemeral_store_arc()
    }
}

impl<R: BookmarksCacheRef> RepoContext<R> {
    /// The warm bookmarks cache for the referenced repository.
    pub fn warm_bookmarks_cache(&self) -> &(dyn BookmarksCache + Send + Sync) {
        self.repo.bookmarks_cache()
    }
}

impl<R: RepoBlobstoreArc> RepoContext<R> {
    /// The repo blobstore for the referenced repository.
    pub fn repo_blobstore(&self) -> ArcRepoBlobstore {
        self.repo.repo_blobstore_arc()
    }
}

impl<R: HookManagerArc> RepoContext<R> {
    /// The hook manager for the referenced repository.
    pub fn hook_manager(&self) -> Arc<HookManager> {
        self.repo.hook_manager_arc()
    }
}

impl<R: RepoHandlerBaseRef> RepoContext<R> {
    /// The base for push redirection logic for this repo
    pub fn maybe_push_redirector_base(&self) -> Option<&PushRedirectorBase> {
        self.repo
            .repo_handler_base()
            .maybe_push_redirector_base
            .as_ref()
            .map(AsRef::as_ref)
    }
}

impl<R: RepoConfigSourceRef> RepoContext<R> {
    /// Where the referenced repository's configuration came from.
    pub fn config_source(&self) -> &RepoConfigSource {
        self.repo.repo_config_source()
    }
}

impl<R: RepoConfigRef> RepoContext<R> {
    /// The configuration for the referenced repository.
    pub fn config(&self) -> &RepoConfig {
        self.repo.repo_config()
    }
}

impl<R: RepoConfigRef + RepoIdentityRef> RepoContext<R> {
    /// Reject commit-cloud writes on repos that accept no drafts at all.
    /// `readonly` alone is not enough: the configerator family is readonly
    /// yet takes drafts. `infinitepush.allow_writes` is the discriminator.
    pub fn ensure_commit_cloud_writable(&self) -> Result<(), MononokeError> {
        if !justknobs::eval(
            "scm/mononoke:reject_commit_cloud_writes_on_readonly_repos",
            None,
            Some(self.name()),
        ) {
            return Ok(());
        }
        if self.config().readonly.is_read_only() && !self.config().infinitepush.allow_writes {
            Err(MononokeError::InvalidRequest(format!(
                "Commit-cloud writes are disabled on readonly repo {} (see scm/mononoke:reject_commit_cloud_writes_on_readonly_repos)",
                self.config().repoid,
            )))
        } else {
            Ok(())
        }
    }
}

impl<R: MutableRenamesArc> RepoContext<R> {
    pub fn mutable_renames(&self) -> ArcMutableRenames {
        self.repo.mutable_renames_arc()
    }
}

impl<R: RepoSparseProfilesArc> RepoContext<R> {
    pub fn sparse_profiles(&self) -> ArcRepoSparseProfiles {
        self.repo.repo_sparse_profiles_arc()
    }
}

impl<R: RepoDerivedDataRef> RepoContext<R> {
    pub fn derive_changeset_info_enabled(&self) -> bool {
        self.repo()
            .repo_derived_data()
            .config()
            .is_enabled(ChangesetInfo::VARIANT)
    }

    pub fn derive_gitcommit_enabled(&self) -> bool {
        self.repo()
            .repo_derived_data()
            .config()
            .is_enabled(MappedGitCommitId::VARIANT)
    }

    pub fn derive_hgchangesets_enabled(&self) -> bool {
        self.repo()
            .repo_derived_data()
            .config()
            .is_enabled(MappedHgChangesetId::VARIANT)
    }

    /// Whether this context is bound to an ephemeral bubble. A bound context
    /// also reads persistent commits, so this does not mean a changeset
    /// read through it is ephemeral.
    pub fn in_bubble(&self) -> bool {
        self.bubble_id().is_some()
    }

    /// The bubble this context is bound to, which can also read persistent commits.
    pub fn bubble_id(&self) -> Option<BubbleId> {
        self.repo().repo_derived_data().manager().bubble_id()
    }
}

impl<R: RepoDerivedDataRef + RepoIdentityRef> RepoContext<R> {
    /// On-demand derivation of `HgAugmentedManifest` at serve-time, gated by both
    /// the derived-data config and a per-repo JustKnob for gradual rollout.
    fn derive_hg_augmented_manifests_enabled(&self) -> bool {
        self.repo()
            .repo_derived_data()
            .config()
            .is_enabled(RootHgAugmentedManifestId::VARIANT)
            && justknobs::eval(
                "scm/mononoke:derive_hg_augmented_manifest_on_demand",
                self.ctx
                    .metadata()
                    .client_request_info()
                    .map(|c| c.correlator.as_str()),
                Some(self.name()),
            )
    }

    /// Whether to derive the `HgAugmentedManifest` when a commit is created in
    /// this repo. Creation-time derivation makes the augmented manifest available
    /// as soon as the commit exists, covering commits that are never resolved
    /// through a serve-time chokepoint (e.g. `hg up` of an already-known commit,
    /// or a commit uploaded then checked out by raw hash). Without it, serving the
    /// augmented manifest in place of the original would fail closed for those
    /// commits.
    pub fn derive_hg_augmented_manifest_at_creation_enabled(&self) -> bool {
        self.repo()
            .repo_derived_data()
            .config()
            .is_enabled(RootHgAugmentedManifestId::VARIANT)
            && justknobs::eval(
                "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset",
                self.ctx
                    .metadata()
                    .client_request_info()
                    .map(|c| c.correlator.as_str()),
                Some(self.name()),
            )
    }

    /// Derive the `HgAugmentedManifest` for a client-influenced batch of
    /// changesets, with bounded concurrency. No-op when disabled for the repo.
    ///
    /// Blocks the caller until every derivation completes — the manifests must
    /// exist before the imminent `trees` fetch, so a fire-and-forget warm-up would
    /// reintroduce the fail-closed race. When the hard failure JK is enabled, a
    /// derivation failure is surfaced to the caller; otherwise failures are logged
    /// and swallowed.
    pub async fn ensure_hg_augmented_manifests_derived(
        &self,
        cs_ids: impl IntoIterator<Item = ChangesetId>,
    ) -> Result<(), MononokeError> {
        if !self.derive_hg_augmented_manifests_enabled() {
            return Ok(());
        }
        let cs_ids: Vec<ChangesetId> = cs_ids.into_iter().collect();
        let results: Vec<Result<(), MononokeError>> = stream::iter(cs_ids)
            .map(|cs_id| self.derive_hg_augmented_manifest(cs_id))
            .buffer_unordered(100)
            .collect()
            .await;
        if justknobs::eval(
            "scm/mononoke:fail_hard_on_derive_hg_augmented_manifest_on_demand",
            self.ctx
                .metadata()
                .client_request_info()
                .map(|c| c.correlator.as_str()),
            Some(self.name()),
        ) {
            results.into_iter().collect::<Result<Vec<_>, _>>()?;
        }
        Ok(())
    }

    /// Derive the `HgAugmentedManifest` for a newly-created commit so it is
    /// present from the moment the commit exists, rather than waiting for the
    /// derived-data tailer to reach it. Best-effort: does nothing when
    /// creation-time derivation is disabled for the repo, and swallows derivation
    /// errors (logged in `derive_hg_augmented_manifest`) so it never blocks or
    /// fails commit creation.
    pub async fn ensure_hg_augmented_manifest_derived_at_creation(&self, cs_id: ChangesetId) {
        if !self.derive_hg_augmented_manifest_at_creation_enabled() {
            return;
        }
        let _ = self.derive_hg_augmented_manifest(cs_id).await;
    }

    /// Derive the `HgAugmentedManifest` for `cs_id`, logging on failure.
    /// Skips bubble/ephemeral commits, whose bubble-scoped derived-data
    /// manager lacks the deps to derive an augmented manifest.
    async fn derive_hg_augmented_manifest(&self, cs_id: ChangesetId) -> Result<(), MononokeError> {
        if self
            .repo()
            .repo_derived_data()
            .manager()
            .bubble_id()
            .is_some()
        {
            return Ok(());
        }
        if let Err(err) = self
            .repo()
            .repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&self.ctx, cs_id, DerivationPriority::LOW)
            .await
        {
            self.ctx.scuba().clone().log_with_msg(
                "HgAugmentedManifest derivation failed",
                Some(format!("cs_id: {cs_id:?}, err: {err:?}")),
            );
            return Err(err.into());
        }
        Ok(())
    }
}

impl<R: CommitGraphRef> RepoContext<R> {
    pub fn commit_graph(&self) -> &CommitGraph {
        self.repo.commit_graph()
    }
}

impl<R: CommitGraphRef + Clone> RepoContext<R> {
    pub async fn many_changeset_parents(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Vec<ChangesetId>>, MononokeError> {
        let parents = self
            .commit_graph()
            .many_changeset_parents(&self.ctx, &changesets)
            .await?
            .into_iter()
            .map(|(cs_id, parents)| (cs_id, parents.to_vec()))
            .collect();
        Ok(parents)
    }

    pub async fn difference_of_unions_of_ancestors<'a>(
        &'a self,
        includes: Vec<ChangesetId>,
        excludes: Vec<ChangesetId>,
    ) -> Result<impl Stream<Item = Result<ChangesetContext<R>, MononokeError>> + 'a, MononokeError>
    {
        let repo = self.clone();

        Ok(self
            .commit_graph()
            .ancestors_difference_stream(&self.ctx, includes, excludes)
            .await?
            .map_ok(move |cs_id| ChangesetContext::new(repo.clone(), cs_id))
            .map_err(|err| err.into()))
    }

    /// A SegmentedChangelog client repository has a compressed shape of the commit graph but
    /// doesn't know the identifiers for all the commits in the graph. It only knows the
    /// identifiers for select commits called "known" commits. These repositories can query
    /// the server to get the identifiers of the commits they don't have using the location
    /// of the desired commit relative to one of the "known" commits.
    /// The current version has all parents of merge commits downloaded to clients so that
    /// locations can be expressed using only the unique descendant distance to one of these
    /// commits. The heads of the repo are also known.
    /// Let's assume our graph is `0 - a - b - c`.
    /// In this example our initial commit is `0`, then we have `a` the first commit, `b` second,
    /// `c` third.
    /// For `descendant = c` and `distance = 2` we want to return `a`.
    pub async fn location_to_changeset_id(
        &self,
        location: Location<ChangesetId>,
        count: u64,
    ) -> Result<Vec<ChangesetId>, MononokeError> {
        let ancestors = self
            .commit_graph()
            .locations_to_changeset_ids(self.ctx(), location.descendant, location.distance, count)
            .await?;

        Ok(ancestors)
    }

    /// A Segmented Changelog client needs to know how to translate between a commit hash,
    /// for example one that is provided by the user, and the information that it has locally,
    /// the shape of the graph, i.e. a location in the graph.
    pub async fn many_changeset_ids_to_locations(
        &self,
        master_heads: Vec<ChangesetId>,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Result<Location<ChangesetId>, MononokeError>>, MononokeError>
    {
        self.commit_graph()
            .changeset_ids_to_locations(self.ctx(), master_heads, cs_ids)
            .await
            .map(|ok| {
                ok.into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            Ok(Location {
                                descendant: v.cs_id,
                                distance: v.distance,
                            }),
                        )
                    })
                    .collect::<HashMap<ChangesetId, Result<_, MononokeError>>>()
            })
            .map_err(MononokeError::from)
    }
}

impl<R: BonsaiGitMappingRef> RepoContext<R> {
    /// Similar to many_changeset_hg_ids, but returning Git-SHA1s.
    pub async fn many_changeset_git_sha1s(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, GitSha1)>, MononokeError> {
        let mapping = self
            .repo()
            .bonsai_git_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.git_sha1))
            .collect();
        Ok(mapping)
    }

    /// Get changeset ID from Git-SHA1 for multiple changesets
    pub async fn many_changeset_ids_from_git_sha1(
        &self,
        changesets: Vec<GitSha1>,
    ) -> Result<Vec<(GitSha1, ChangesetId)>, MononokeError> {
        let mapping = self
            .repo()
            .bonsai_git_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.git_sha1, entry.bcs_id))
            .collect();
        Ok(mapping)
    }
}

impl<R: BonsaiGlobalrevMappingRef> RepoContext<R> {
    /// Similar to many_changeset_hg_ids, but returning Globalrevs.
    pub async fn many_changeset_globalrev_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, Globalrev)>, MononokeError> {
        let mapping = self
            .repo()
            .bonsai_globalrev_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.globalrev))
            .collect();
        Ok(mapping)
    }

    /// Get changeset ID from Globalrev for multiple changesets
    pub async fn many_changeset_ids_from_globalrev(
        &self,
        changesets: Vec<Globalrev>,
    ) -> Result<Vec<(Globalrev, ChangesetId)>, MononokeError> {
        let mapping = self
            .repo()
            .bonsai_globalrev_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.globalrev, entry.bcs_id))
            .collect();
        Ok(mapping)
    }
}

impl<R: BonsaiSvnrevMappingRef> RepoContext<R> {
    /// Similar to many_changeset_hg_ids, but returning Svnrevs.
    pub async fn many_changeset_svnrev_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, Svnrev)>, MononokeError> {
        let mapping = self
            .repo()
            .bonsai_svnrev_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.svnrev))
            .collect();
        Ok(mapping)
    }
}

impl<R: BonsaiP4MappingRef> RepoContext<R> {
    /// Similar to many_changeset_hg_ids, but returning Perforce changelist ids.
    pub async fn many_changeset_p4_changelist_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, P4ChangelistId)>, MononokeError> {
        let mapping = self
            .repo()
            .bonsai_p4_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.p4_changelist_id))
            .collect();
        Ok(mapping)
    }
}

impl<R: RepoConfigRef> RepoContext<R> {
    /// Start a write to the repo.
    pub fn start_write(&self) -> Result<(), MononokeError> {
        if self.authz.is_service() {
            if !self.config().source_control_service.permit_service_writes {
                return Err(MononokeError::InvalidRequest(String::from(
                    "Service writes are disabled in configuration for this repo",
                )));
            }
        } else if !self.config().source_control_service.permit_writes {
            return Err(MononokeError::InvalidRequest(String::from(
                "Writes are disabled in configuration for this repo",
            )));
        }

        self.ctx
            .scuba()
            .clone()
            .log_with_msg("Write request start", None);

        Ok(())
    }

    /// Reads a value out of the underlying config, indicating if we support writes without parents in this repo.
    pub fn allow_no_parent_writes(&self) -> bool {
        self.config()
            .source_control_service
            .permit_commits_without_parents
    }
}

impl<R: Clone> RepoContext<R> {
    /// Create changeset context from known existing changeset id.
    pub fn changeset_from_existing_id(&self, cs_id: ChangesetId) -> ChangesetContext<R> {
        ChangesetContext::new(self.clone(), cs_id)
    }

    fn target_repo(&self) -> Target<R> {
        Target(self.repo().clone())
    }
}

impl<R> RepoContext<R>
where
    R: RepoPermissionCheckerRef
        + RepoIdentityRef
        + RepoEphemeralStoreRef
        + RepoWithBubble
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub async fn new(
        ctx: CoreContext,
        authz: Arc<AuthorizationContext>,
        repo: Arc<R>,
        bubble_id: Option<BubbleId>,
        repos: Arc<MononokeRepos<R>>,
    ) -> Result<Self, MononokeError> {
        let ctx = ctx.with_mutated_scuba(|mut scuba| {
            scuba.add("permissions_model", format!("{authz:?}"));
            scuba
        });

        // Check the user is permitted to access this repo.
        let repo_acl_deciding_identity_type = authz
            .require_repo_metadata_read_with_result(&ctx, &repo)
            .await?;

        // Open the bubble if necessary.
        let repo = if let Some(bubble_id) = bubble_id {
            let bubble = repo
                .repo_ephemeral_store()
                .open_bubble(&ctx, bubble_id)
                .await?;
            Arc::new(repo.with_bubble(bubble))
        } else {
            repo
        };

        Ok(Self {
            ctx,
            authz,
            repo,
            push_redirector: LazyShared::new_empty(),
            repos,
            repo_acl_deciding_identity_type,
            path_acl_deciding_identity_types: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub async fn new_test(ctx: CoreContext, repo: Arc<R>) -> Result<Self, MononokeError> {
        let authz = Arc::new(AuthorizationContext::new_bypass_access_control());
        RepoContext::new(ctx, authz, repo, None, Arc::new(MononokeRepos::new())).await
    }
}

impl<R: MononokeRepo> RepoContext<R> {
    /// The push redirector for this repo, if one is configured.
    ///
    /// Resolved lazily on first use and cached for the lifetime of the
    /// `RepoContext`. Returns `None` if this repo isn't configured for push
    /// redirection.
    pub async fn push_redirector(&self) -> Result<Option<Arc<PushRedirector<R>>>, MononokeError> {
        self.push_redirector
            .get_or_init(|| {
                let ctx = self.ctx.clone();
                let repo = self.repo.clone();
                let repos = self.repos.clone();
                async move {
                    Ok(maybe_push_redirector(&ctx, &repo, repos.as_ref())
                        .await?
                        .map(Arc::new))
                }
            })
            .await
    }
}

impl<R> RepoContext<R>
where
    R: RepoIdentityRef
        + RepoBlobstoreRef
        + RepoEphemeralStoreRef
        + RepoWithBubble
        + CommitGraphRef
        + CommitGraphArc
        + BonsaiHgMappingRef
        + BonsaiGitMappingRef
        + BonsaiGlobalrevMappingRef
        + BonsaiSvnrevMappingRef
        + BonsaiP4MappingRef
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Load bubble from id
    pub async fn open_bubble(&self, bubble_id: BubbleId) -> Result<Bubble, MononokeError> {
        Ok(self
            .repo
            .repo_ephemeral_store()
            .open_bubble(self.ctx(), bubble_id)
            .await?)
    }

    async fn commit_graph_for_bubble(
        &self,
        bubble_id: Option<BubbleId>,
    ) -> Result<ArcCommitGraph, MononokeError> {
        Ok(match bubble_id {
            Some(id) => Arc::new(self.open_bubble(id).await?.repo_commit_graph(self.repo())),
            None => self.repo().commit_graph_arc(),
        })
    }

    /// Test whether a changeset exists in a particular storage location.
    pub async fn changeset_exists(
        &self,
        changeset_id: ChangesetId,
        storage_location: StorageLocation,
    ) -> Result<bool, MononokeError> {
        use StorageLocation::*;
        let bubble_id = match storage_location {
            Persistent => None,
            Bubble(id) => Some(id),
            UnknownBubble => match self
                .repo
                .repo_ephemeral_store()
                .bubble_from_changeset(&self.ctx, &changeset_id)
                .await?
            {
                Some(id) => Some(id),
                None => return Ok(false),
            },
        };
        Ok(self
            .commit_graph_for_bubble(bubble_id)
            .await?
            .exists(&self.ctx, changeset_id)
            .await?)
    }

    /// Look up a changeset specifier to find the canonical bonsai changeset
    /// ID for a changeset.
    pub async fn resolve_specifier(
        &self,
        specifier: ChangesetSpecifier,
    ) -> Result<Option<ChangesetId>, MononokeError> {
        let id = match specifier {
            ChangesetSpecifier::Bonsai(cs_id) => self
                .changeset_exists(cs_id, StorageLocation::Persistent)
                .await?
                .then_some(cs_id),
            ChangesetSpecifier::EphemeralBonsai(cs_id, bubble_id) => self
                .changeset_exists(cs_id, StorageLocation::ephemeral(bubble_id))
                .await?
                .then_some(cs_id),
            ChangesetSpecifier::Hg(hg_cs_id) => {
                self.repo()
                    .bonsai_hg_mapping()
                    .get_bonsai_from_hg(&self.ctx, hg_cs_id)
                    .await?
            }
            ChangesetSpecifier::Globalrev(rev) => {
                self.repo()
                    .bonsai_globalrev_mapping()
                    .get_bonsai_from_globalrev(&self.ctx, rev)
                    .await?
            }
            ChangesetSpecifier::Svnrev(rev) => {
                self.repo()
                    .bonsai_svnrev_mapping()
                    .get_bonsai_from_svnrev(&self.ctx, rev)
                    .await?
            }
            ChangesetSpecifier::P4Changelist(cl) => {
                self.repo()
                    .bonsai_p4_mapping()
                    .get_bonsai_from_p4_changelist_id(&self.ctx, cl)
                    .await?
            }
            ChangesetSpecifier::GitSha1(git_sha1) => {
                self.repo()
                    .bonsai_git_mapping()
                    .get_bonsai_from_git_sha1(&self.ctx, git_sha1)
                    .await?
            }
        };
        Ok(id)
    }

    /// Resolve a changeset id by its prefix
    pub async fn resolve_changeset_id_prefix(
        &self,
        prefix: ChangesetPrefixSpecifier,
    ) -> Result<ChangesetSpecifierPrefixResolution, MononokeError> {
        const MAX_LIMIT_AMBIGUOUS_IDS: usize = 10;
        let resolved = match prefix {
            ChangesetPrefixSpecifier::Hg(prefix) => ChangesetSpecifierPrefixResolution::from(
                self.repo()
                    .bonsai_hg_mapping()
                    .get_many_hg_by_prefix(&self.ctx, prefix, MAX_LIMIT_AMBIGUOUS_IDS)
                    .await?,
            ),
            ChangesetPrefixSpecifier::Bonsai(prefix) => ChangesetSpecifierPrefixResolution::from(
                self.repo()
                    .commit_graph()
                    .find_by_prefix(&self.ctx, prefix, MAX_LIMIT_AMBIGUOUS_IDS)
                    .await?,
            ),
            ChangesetPrefixSpecifier::GitSha1(prefix) => ChangesetSpecifierPrefixResolution::from(
                self.repo()
                    .bonsai_git_mapping()
                    .get_many_git_sha1_by_prefix(&self.ctx, prefix, MAX_LIMIT_AMBIGUOUS_IDS)
                    .await?,
            ),
            ChangesetPrefixSpecifier::Globalrev(prefix) => {
                ChangesetSpecifierPrefixResolution::from(
                    self.repo()
                        .bonsai_globalrev_mapping()
                        .get_closest_globalrev(&self.ctx, prefix)
                        .await?,
                )
            }
        };
        Ok(resolved)
    }

    /// Look up a changeset by specifier.
    pub async fn changeset(
        &self,
        specifier: impl Into<ChangesetSpecifier>,
    ) -> Result<Option<ChangesetContext<R>>, MononokeError> {
        let specifier = specifier.into();
        let changeset = self
            .resolve_specifier(specifier)
            .await?
            .map(|cs_id| ChangesetContext::new(self.clone(), cs_id));
        Ok(changeset)
    }
}

impl<R: MononokeRepo> RepoContext<R> {
    /// Resolve a bookmark to a changeset.
    pub async fn resolve_bookmark(
        &self,
        bookmark: &BookmarkKey,
        freshness: BookmarkFreshness,
    ) -> Result<Option<ChangesetContext<R>>, MononokeError> {
        let mut cs_id = match freshness {
            BookmarkFreshness::MaybeStale => {
                self.warm_bookmarks_cache().get(&self.ctx, bookmark).await?
            }
            BookmarkFreshness::MostRecent => None,
        };

        // If the bookmark wasn't found in the warm bookmarks cache, it might
        // be a scratch bookmark, so always do the look-up.
        if cs_id.is_none() {
            cs_id = self
                .repo()
                .bookmarks()
                .get(self.ctx.clone(), bookmark, freshness)
                .await?
        }

        Ok(cs_id.map(|cs_id| ChangesetContext::new(self.clone(), cs_id)))
    }

    /// Get Mercurial ID for multiple changesets
    ///
    /// This is a more efficient version of:
    /// ```ignore
    /// let ids: Vec<ChangesetId> = ...;
    /// ids.into_iter().map(|id| {
    ///     let hg_id = repo
    ///         .changeset(ChangesetSpecifier::Bonsai(id))
    ///         .await
    ///         .hg_id();
    ///     (id, hg_id)
    /// });
    /// ```
    pub async fn many_changeset_hg_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, HgChangesetId)>, MononokeError> {
        let mapping: Vec<(ChangesetId, HgChangesetId)> = self
            .repo()
            .get_hg_bonsai_mapping(self.ctx.clone(), changesets)
            .await?
            .into_iter()
            .map(|(hg_cs_id, cs_id)| (cs_id, hg_cs_id))
            .collect();
        self.ensure_hg_augmented_manifests_derived(mapping.iter().map(|(cs_id, _)| *cs_id))
            .await?;
        Ok(mapping)
    }

    /// Get changeset ID from Mercurial ID for multiple changesets
    pub async fn many_changeset_ids_from_hg(
        &self,
        changesets: Vec<HgChangesetId>,
    ) -> Result<Vec<(HgChangesetId, ChangesetId)>, MononokeError> {
        let mapping = self
            .repo()
            .get_hg_bonsai_mapping(self.ctx.clone(), changesets)
            .await?;
        Ok(mapping)
    }

    /// Return comprehensive bookmark info including last update time
    /// Currently works only for public bookmarks.
    pub async fn bookmark_info(
        &self,
        bookmark: impl AsRef<str>,
    ) -> Result<Option<BookmarkInfo<R>>, MononokeError> {
        // a non ascii bookmark name is an invalid request
        let bookmark = BookmarkKey::new(bookmark.as_ref())
            .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?;

        let (maybe_warm_cs_id, maybe_log_entry) = try_join!(
            self.warm_bookmarks_cache().get(&self.ctx, &bookmark),
            async {
                let mut entries_stream =
                    self.repo().bookmark_update_log().list_bookmark_log_entries(
                        self.ctx.clone(),
                        bookmark.clone(),
                        1,
                        None,
                        Freshness::MaybeStale,
                    );
                entries_stream.next().await.transpose()
            }
        )?;

        let maybe_warm_changeset =
            maybe_warm_cs_id.map(|cs_id| ChangesetContext::new(self.clone(), cs_id));

        let (maybe_fresh_cs_id, timestamp) = match maybe_log_entry {
            Some((_id, maybe_fresh_cs_id, _reason, timestamp)) => (maybe_fresh_cs_id, timestamp),
            None => {
                return Ok(None);
            }
        };

        let fresh_cs_id = match maybe_fresh_cs_id {
            Some(cs_id) => cs_id,
            None => {
                return Ok(None);
            }
        };
        let fresh_changeset = ChangesetContext::new(self.clone(), fresh_cs_id);

        let last_update_timestamp = timestamp;

        // If the bookmark wasn't found in the warm bookmarks cache return
        // the fresh value for simplicity.
        let warm_changeset = maybe_warm_changeset.unwrap_or_else(|| fresh_changeset.clone());

        Ok(Some(BookmarkInfo {
            warm_changeset,
            fresh_changeset,
            last_update_timestamp,
        }))
    }

    /// Get a list of bookmarks.
    pub async fn list_bookmarks(
        &self,
        include_scratch: bool,
        prefix: Option<&str>,
        after: Option<&str>,
        limit: Option<u64>,
    ) -> Result<
        impl Stream<Item = Result<(String, ChangesetId), MononokeError>> + use<'_, R>,
        MononokeError,
    > {
        if include_scratch {
            if prefix.is_none() {
                return Err(MononokeError::InvalidRequest(
                    "prefix required to list scratch bookmarks".to_string(),
                ));
            }
            if limit.is_none() {
                return Err(MononokeError::InvalidRequest(
                    "limit required to list scratch bookmarks".to_string(),
                ));
            }
        }

        let prefix = match prefix {
            Some(prefix) => BookmarkPrefix::new(prefix).map_err(|e| {
                MononokeError::InvalidRequest(format!("invalid bookmark prefix '{prefix}': {e}"))
            })?,
            None => BookmarkPrefix::empty(),
        };

        let pagination = match after {
            Some(after) => {
                let name = BookmarkName::new(after).map_err(|e| {
                    MononokeError::InvalidRequest(format!("invalid bookmark name '{after}': {e}"))
                })?;
                BookmarkPagination::After(name)
            }
            None => BookmarkPagination::FromStart,
        };

        if include_scratch {
            // Scratch bookmarks must be queried directly from the blobrepo as
            // they are not stored in the cache.  To maintain ordering with
            // public bookmarks, query all the bookmarks we are interested in.
            let repo = self.repo();
            let cache = self.warm_bookmarks_cache();
            let bookmarks = repo
                .bookmarks()
                .list(
                    self.ctx.clone(),
                    BookmarkFreshness::MaybeStale,
                    &prefix,
                    BookmarkCategory::ALL,
                    BookmarkKind::ALL,
                    &pagination,
                    limit.unwrap_or(u64::MAX),
                )
                .try_filter_map(move |(bookmark, cs_id)| async move {
                    tokio::task::yield_now().await;
                    if bookmark.kind() == &BookmarkKind::Scratch {
                        Ok(Some((bookmark.into_key().into_string(), cs_id)))
                    } else {
                        // For non-scratch bookmarks, always return the value
                        // from the cache so that clients only ever see the
                        // warm value.  If the bookmark is newly created and
                        // has no warm value, this might mean we have to
                        // filter this bookmark out.
                        let bookmark_name = bookmark.into_key();
                        let maybe_cs_id = cache.get(&self.ctx, &bookmark_name).watched().await?;
                        Ok(maybe_cs_id.map(|cs_id| (bookmark_name.into_string(), cs_id)))
                    }
                })
                .map_err(MononokeError::from)
                .boxed();
            Ok(bookmarks)
        } else {
            // Public bookmarks can be fetched from the warm bookmarks cache.
            let cache = self.warm_bookmarks_cache();
            Ok(stream::iter(
                cache
                    .list(&self.ctx, &prefix, &pagination, limit)
                    .watched()
                    .await?,
            )
            .map(|(bookmark, (cs_id, _kind))| Ok((bookmark.into_string(), cs_id)))
            .boxed())
        }
    }
}

impl<
    R: RepoBlobstoreRef
        + RestrictedPathsArc
        + RepoPermissionCheckerRef
        + RepoIdentityRef
        + Clone
        + Send
        + Sync
        + 'static,
> RepoContext<R>
{
    /// Get a Tree by id.  Returns `None` if the tree doesn't exist.
    pub async fn tree(
        &self,
        tree_id: ContentManifestId,
    ) -> Result<Option<TreeContext<R>>, MononokeError> {
        TreeContext::new_check_exists(self.clone(), tree_id).await
    }
}

impl<
    R: RepoBlobstoreRef + RepoPermissionCheckerRef + RepoIdentityRef + Clone + Send + Sync + 'static,
> RepoContext<R>
{
    /// Get a File by id.  Returns `None` if the file doesn't exist.
    pub async fn file(&self, file_id: FileId) -> Result<Option<FileContext<R>>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Canonical(file_id)).await
    }

    /// Get a File by content sha-1.  Returns `None` if the file doesn't exist.
    pub async fn file_by_content_sha1(
        &self,
        hash: Sha1,
    ) -> Result<Option<FileContext<R>>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::Sha1(hash))).await
    }

    /// Get a File by content sha-256.  Returns `None` if the file doesn't exist.
    pub async fn file_by_content_sha256(
        &self,
        hash: Sha256,
    ) -> Result<Option<FileContext<R>>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::Sha256(hash))).await
    }

    /// Get a File by content git-sha-1.  Returns `None` if the file doesn't exist.
    pub async fn file_by_content_gitsha1(
        &self,
        hash: GitSha1,
    ) -> Result<Option<FileContext<R>>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::GitSha1(hash))).await
    }

    /// Get a File by content seeded-blake3. Returns `None` if the file doesn't exist.
    pub async fn file_by_content_seeded_blake3(
        &self,
        hash: Blake3,
    ) -> Result<Option<FileContext<R>>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::SeededBlake3(hash)))
            .await
    }
}

impl<R: RepoBlobstoreRef + FilestoreConfigRef> RepoContext<R> {
    pub async fn upload_file_content(
        &self,
        content: Bytes,
        store_request: &StoreRequest,
    ) -> Result<ContentId, MononokeError> {
        let metadata = filestore::store(
            self.repo.repo_blobstore(),
            *self.repo.filestore_config(),
            &self.ctx,
            store_request,
            stream::once(async move { Ok(content) }),
        )
        .await?;
        Ok(metadata.content_id)
    }
}

impl<R: PhasesRef + CommitGraphRef + Clone> RepoContext<R> {
    /// Get a stack for the list of heads (up to the first public commit).
    ///
    /// Limit constrains the number of draft commits returned.
    /// Algo is designed to minimize number of db queries.
    /// Missing changesets are skipped.
    /// Changesets are returned in topological order (requested heads first)
    ///
    /// When the limit is reached returns the heads of which children were not processed to allow
    /// for continuation of the processing.
    pub async fn stack(
        &self,
        changesets: Vec<ChangesetId>,
        limit: usize,
    ) -> Result<Stack, MononokeError> {
        if limit == 0 {
            return Ok(Default::default());
        }

        let walk_start = Instant::now();

        // initialize visited
        let mut visited: HashSet<_> = changesets.iter().cloned().collect();

        let phases = self.repo().phases();

        // get phases
        let public_phases = phases
            .get_public(&self.ctx, changesets.clone(), false)
            .await?;

        // partition
        let (mut public, mut draft): (Vec<_>, Vec<_>) = changesets
            .into_iter()
            .partition(|cs_id| public_phases.contains(cs_id));

        // initialize the queue
        let mut queue: Vec<_> = draft.to_vec();

        while !queue.is_empty() {
            // get the unique parents for all changesets in the queue & skip visited & update visited
            let parents: Vec<_> = self
                .commit_graph()
                .many_changeset_parents(&self.ctx, &queue)
                .await?
                .into_values()
                .flatten()
                .filter(|cs_id| !visited.contains(cs_id))
                .unique()
                .collect();

            visited.extend(parents.iter().cloned());

            // get phases for the parents
            let public_phases = phases.get_public(&self.ctx, parents.clone(), false).await?;

            // partition
            let (new_public, new_draft): (Vec<_>, Vec<_>) = parents
                .into_iter()
                .partition(|cs_id| public_phases.contains(cs_id));

            // respect the limit
            if draft.len() + new_draft.len() > limit {
                break;
            }

            // update queue and level
            queue = new_draft.clone();

            // update draft & public
            public.extend(new_public);
            draft.extend(new_draft);
        }

        self.ctx
            .perf_counters()
            .add_duration_us(PerfCounterType::PhasesSectionUs, walk_start.elapsed());

        Ok(Stack {
            draft,
            public,
            leftover_heads: queue,
        })
    }
}

impl<R: MononokeRepo> RepoContext<R> {
    async fn build_candidate_selection_hint(
        &self,
        maybe_args: Option<CandidateSelectionHintArgs>,
        other_repo_context: &Self,
    ) -> Result<CandidateSelectionHint<R>, MononokeError> {
        let args = match maybe_args {
            None => return Ok(CandidateSelectionHint::Only),
            Some(args) => args,
        };

        use CandidateSelectionHintArgs::*;
        match args {
            AncestorOfBookmark(bookmark) => {
                let repo = other_repo_context.target_repo();
                Ok(CandidateSelectionHint::AncestorOfBookmark(
                    Target(bookmark),
                    repo,
                ))
            }
            DescendantOfBookmark(bookmark) => {
                let repo = other_repo_context.target_repo();
                Ok(CandidateSelectionHint::DescendantOfBookmark(
                    Target(bookmark),
                    repo,
                ))
            }
            AncestorOfCommit(specifier) => {
                let repo = other_repo_context.target_repo();
                let cs_id = other_repo_context
                    .resolve_specifier(specifier)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "unknown commit specifier {specifier}"
                        ))
                    })?;
                Ok(CandidateSelectionHint::AncestorOfCommit(
                    Target(cs_id),
                    repo,
                ))
            }
            DescendantOfCommit(specifier) => {
                let repo = other_repo_context.target_repo();
                let cs_id = other_repo_context
                    .resolve_specifier(specifier)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "unknown commit specifier {specifier}"
                        ))
                    })?;
                Ok(CandidateSelectionHint::DescendantOfCommit(
                    Target(cs_id),
                    repo,
                ))
            }
            Exact(specifier) => {
                let cs_id = other_repo_context
                    .resolve_specifier(specifier)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "unknown commit specifier {specifier}"
                        ))
                    })?;
                Ok(CandidateSelectionHint::Exact(Target(cs_id)))
            }
        }
    }

    /// Get the equivalent changeset from another repo - it may sync it if needed (depending on
    /// `sync_behaviour` arg).
    ///
    /// Setting exact to true will return result only if there's exact match for the requested
    /// commit - rather than commit with equivalent working copy (which happens in case the source
    /// commit rewrites to nothing in target repo).
    pub async fn xrepo_commit_lookup(
        &self,
        other: &Self,
        specifier: impl Into<ChangesetSpecifier>,
        maybe_candidate_selection_hint_args: Option<CandidateSelectionHintArgs>,
        sync_behaviour: XRepoLookupSyncBehaviour,
        exact: XRepoLookupExactBehaviour,
    ) -> Result<Option<ChangesetContext<R>>, MononokeError>
    where
        R: Clone,
    {
        let candidate_selection_hint: CandidateSelectionHint<R> = self
            .build_candidate_selection_hint(maybe_candidate_selection_hint_args, other)
            .await?;

        let (_small_repo, _large_repo) =
            get_small_and_large_repos(self.repo.as_ref(), other.repo.as_ref())?;

        let repo_provider: RepoProvider<'_, R> = Arc::new(move |repo_id| {
            Box::pin({
                let repos = self.repos.clone();

                async move {
                    let repo = repos
                        .get_by_id(repo_id.id())
                        .ok_or_else(|| anyhow!("Submodule dependency repo with id {repo_id} not available through RepoContext"))?;
                    Ok(repo)
                }
            })
        });

        let submodule_deps = get_all_submodule_deps_from_repo_pair(
            &self.ctx,
            self.repo.clone(),
            other.repo.clone(),
            repo_provider,
        )
        .await?;

        let commit_sync_repos = CommitSyncRepos::from_source_and_target_repos(
            self.repo().clone(),
            other.repo().clone(),
            submodule_deps,
        )?;

        let specifier = specifier.into();
        let changeset = self.resolve_specifier(specifier).await?.ok_or_else(|| {
            MononokeError::InvalidRequest(format!("unknown commit specifier {specifier}"))
        })?;

        let commit_sync_data =
            CommitSyncData::new(&self.ctx, commit_sync_repos, self.live_commit_sync_config());

        if sync_behaviour == XRepoLookupSyncBehaviour::SyncIfAbsent {
            let _ = sync_commit(
                &self.ctx,
                changeset,
                &commit_sync_data,
                candidate_selection_hint,
                CommitSyncContext::ScsXrepoLookup,
                false,
            )
            .await?;
        }
        use cross_repo_sync::CommitSyncOutcome::*;
        let maybe_cs_id = commit_sync_data
            .get_commit_sync_outcome(&self.ctx, changeset)
            .await?
            .and_then(|outcome| match outcome {
                NotSyncCandidate(_) => None,
                EquivalentWorkingCopyAncestor(_cs_id, _)
                    if exact == XRepoLookupExactBehaviour::OnlyExactMapping =>
                {
                    None
                }
                EquivalentWorkingCopyAncestor(cs_id, _) | RewrittenAs(cs_id, _) => Some(cs_id),
            });
        Ok(maybe_cs_id.map(|cs_id| ChangesetContext::new(other.clone(), cs_id)))
    }

    /// This provides the same functionality as
    /// `mononoke_api::RepoContext::location_to_changeset_id`. It just wraps the request and
    /// response using Git specific types.
    pub async fn location_to_git_changeset_id(
        &self,
        location: Location<GitSha1>,
        count: u64,
    ) -> Result<Vec<GitSha1>, MononokeError> {
        let cs_location = location
            .and_then_descendant(|descendant| async move {
                self.repo()
                    .bonsai_git_mapping()
                    .get_bonsai_from_git_sha1(self.ctx(), descendant)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "git changeset {descendant} not found"
                        ))
                    })
            })
            .await?;
        let result_csids = self.location_to_changeset_id(cs_location, count).await?;
        let git_id_futures = result_csids.iter().map(|result_csid| {
            derive_git_changeset(self.ctx(), self.repo().repo_derived_data(), *result_csid)
        });
        future::try_join_all(git_id_futures)
            .await
            .map_err(MononokeError::from)
    }

    /// This provides the same functionality as
    /// `mononoke_api::RepoContext::many_changeset_ids_to_locations`. It just translates to
    /// and from Git types.
    pub async fn many_git_commit_ids_to_locations(
        &self,
        git_master_heads: Vec<GitSha1>,
        git_ids: Vec<GitSha1>,
    ) -> Result<HashMap<GitSha1, Result<Location<GitSha1>, MononokeError>>, MononokeError> {
        let all_git_ids: Vec<_> = git_ids
            .iter()
            .cloned()
            .chain(git_master_heads.clone())
            .collect();
        let git_to_bonsai: HashMap<GitSha1, ChangesetId> =
            get_git_bonsai_mapping(self.ctx().clone(), self, all_git_ids)
                .await?
                .into_iter()
                .collect();
        let master_heads = git_master_heads
            .iter()
            .map(|master_id| {
                git_to_bonsai.get(master_id).cloned().ok_or_else(|| {
                    MononokeError::InvalidRequest(format!(
                        "failed to find bonsai equivalent for client head {master_id}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, MononokeError>>()?;

        // We should treat git_ids as being absolutely any hash. It is perfectly valid for the
        // server to have not encountered the hash that it was given to convert. Filter out the
        // hashes that we could not convert to bonsai.
        let cs_ids = git_ids
            .iter()
            .filter_map(|hg_id| git_to_bonsai.get(hg_id).cloned())
            .collect::<Vec<ChangesetId>>();

        let cs_to_blocations = self
            .many_changeset_ids_to_locations(master_heads, cs_ids)
            .await?;

        let bonsai_to_git: HashMap<ChangesetId, GitSha1> = get_git_bonsai_mapping(
            self.ctx().clone(),
            self,
            cs_to_blocations
                .values()
                .filter_map(|result| match result {
                    Ok(l) => Some(l.descendant),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )
        .await?
        .into_iter()
        .map(|(git_id, cs_id)| (cs_id, git_id))
        .collect();
        let response = git_ids
            .into_iter()
            .filter_map(|git_id| git_to_bonsai.get(&git_id).map(|cs_id| (git_id, cs_id)))
            .filter_map(|(git_id, cs_id)| {
                cs_to_blocations
                    .get(cs_id)
                    .map(|cs_result| (git_id, cs_result.clone()))
            })
            .map(|(git_id, cs_result)| {
                let cs_result = match cs_result {
                    Ok(cs_location) => cs_location.try_map_descendant(|descendant| {
                        bonsai_to_git.get(&descendant).cloned().ok_or_else(|| {
                            MononokeError::InvalidRequest(format!(
                                "failed to find git equivalent for bonsai {descendant}"
                            ))
                        })
                    }),
                    Err(e) => Err(e),
                };
                (git_id, cs_result)
            })
            .collect::<HashMap<GitSha1, Result<Location<GitSha1>, MononokeError>>>();

        Ok(response)
    }
}

impl<R: RepoDerivedDataRef> RepoContext<R> {
    // TODO(mbthomas): get_git_from_bonsai -> derive_git_changeset
    pub async fn get_git_from_bonsai(&self, cs_id: ChangesetId) -> Result<GitSha1, MononokeError> {
        Ok(derive_git_changeset(self.ctx(), self.repo().repo_derived_data(), cs_id).await?)
    }

    pub async fn derive_bulk_locally(
        &self,
        ctx: &CoreContext,
        csids: Vec<ChangesetId>,
        derivable_types: &[DerivableType],
        override_batch_size: Option<u64>,
    ) -> Result<(), MononokeError> {
        // We don't need to expose rederivation to users of the repo api
        // That's a lower level concept that clients like the derived data backfiller
        // can get straight from the derived data manager
        let rederivation = None;
        Ok(self
            .repo
            .repo_derived_data()
            .manager()
            .derive_bulk_locally(
                ctx,
                &csids,
                rederivation,
                derivable_types,
                override_batch_size,
                None,
            )
            .await?)
    }

    pub async fn is_derived(
        &self,
        ctx: &CoreContext,
        csid: ChangesetId,
        derivable_type: DerivableType,
    ) -> Result<bool, MononokeError> {
        // We don't need to expose rederivation to users of the repo api
        // That's a lower level concept that clients like the derived data backfiller
        // can get straight from the derived data manager
        let rederivation = None;
        Ok(self
            .repo
            .repo_derived_data()
            .manager()
            .is_derived(ctx, csid, rederivation, derivable_type)
            .await?)
    }
}

impl<R: RepoIdentityRef> PartialEq for RepoContext<R> {
    fn eq(&self, other: &Self) -> bool {
        self.repoid() == other.repoid()
    }
}
impl<R: RepoIdentityRef> Eq for RepoContext<R> {}

impl<R: RepoIdentityRef> Hash for RepoContext<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.repoid().hash(state);
    }
}

// TODO(mbthomas): This is temporary to allow us to derive git changesets
pub async fn derive_git_changeset(
    ctx: &CoreContext,
    derived_data: &RepoDerivedData,
    cs_id: ChangesetId,
) -> Result<GitSha1, Error> {
    match derived_data
        .derive::<MappedGitCommitId>(ctx, cs_id, DerivationPriority::LOW)
        .await
    {
        Ok(id) => Ok(*id.oid()),
        Err(err) => Err(err.into()),
    }
}

// TODO(mbthomas): This is temporary to allow us to derive git changesets
// Returns only the mapping for valid changesets that are known to the server.
// For Bonsai -> Git conversion, missing Git changesets will be derived (so all Bonsais will be
// in the output).
// For Git -> Bonsai conversion, missing Bonsais will not be returned, since they cannot be
// derived from Git Changesets.
async fn get_git_bonsai_mapping<'a, R>(
    ctx: CoreContext,
    repo_ctx: &RepoContext<R>,
    bonsai_or_git_shas: impl Into<BonsaisOrGitShas> + 'a + Send,
) -> Result<Vec<(GitSha1, ChangesetId)>, Error>
where
    //R: CommitGraphRef + RepoDerivedDataRef + BonsaiHgMappingRef,
    R: MononokeRepo,
{
    // STATS::get_git_bonsai_mapping.add_value(1);

    let bonsai_or_git_shas = bonsai_or_git_shas.into();
    let git_bonsai_list = repo_ctx
        .repo()
        .bonsai_git_mapping()
        .get(&ctx, bonsai_or_git_shas.clone())
        .await?
        .into_iter()
        .map(|entry| (entry.git_sha1, entry.bcs_id))
        .collect::<Vec<_>>();

    use BonsaisOrGitShas::*;
    match bonsai_or_git_shas {
        Bonsai(bonsais) => {
            // If a bonsai commit doesn't exist in the bonsai_git_mapping,
            // that might mean two things: 1) Bonsai commit just doesn't exist
            // 2) Bonsai commit exists but git changesets weren't generated for it
            // Normally the callers of get_git_bonsai_mapping would expect that git
            // changesets will be lazily generated, so the
            // code below explicitly checks if a commit exists and if yes then
            // generates git changeset for it.
            let mapping: HashMap<_, _> = git_bonsai_list
                .iter()
                .map(|(git_id, bcs_id)| (bcs_id, git_id))
                .collect();

            let mut notfound = vec![];
            for b in bonsais {
                if !mapping.contains_key(&b) {
                    notfound.push(b);
                }
            }

            let existing: HashSet<_> = repo_ctx
                .commit_graph()
                .known_changesets(&ctx, notfound.clone())
                .await?
                .into_iter()
                .collect();

            let mut newmapping: Vec<_> = stream::iter(
                notfound
                    .into_iter()
                    .filter(|csid| existing.contains(csid))
                    .map(Ok),
            )
            .map_ok(|csid| {
                derive_git_changeset(&ctx, repo_ctx.repo().repo_derived_data(), csid)
                    .map_ok(move |gitsha1| (gitsha1, csid))
            })
            .try_buffer_unordered(100)
            .try_collect()
            .await?;

            newmapping.extend(git_bonsai_list);
            Ok(newmapping)
        }
        GitSha1(_) => Ok(git_bonsai_list),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use fbinit::FacebookInit;
    use fixtures::Linear;
    use fixtures::MergeEven;
    use fixtures::TestRepoFixture;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::fbinit_test]
    async fn test_try_find_child(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = Linear::get_repo(fb).await;

        let ancestor = ChangesetId::from_str(
            "c9f9a2a39195a583d523a4e5f6973443caeb0c66a315d5bf7db1b5775c725310",
        )?;
        let descendant = ChangesetId::from_str(
            "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6",
        )?;

        let maybe_child = try_find_child(&ctx, &repo, ancestor, descendant, 100).await?;
        let child = maybe_child.ok_or_else(|| anyhow!("didn't find child"))?;
        assert_eq!(
            child,
            ChangesetId::from_str(
                "98ef3234c2f1acdbb272715e8cfef4a6378e5443120677e0d87d113571280f79"
            )?
        );

        let maybe_child = try_find_child(&ctx, &repo, ancestor, descendant, 1).await?;
        assert!(maybe_child.is_none());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_try_find_child_merge(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = MergeEven::get_repo(fb).await;

        let ancestor = ChangesetId::from_str(
            "35fb4e0fb3747b7ca4d18281d059be0860d12407dc5dce5e02fb99d1f6a79d2a",
        )?;
        let descendant = ChangesetId::from_str(
            "567a25d453cafaef6550de955c52b91bf9295faf38d67b6421d5d2e532e5adef",
        )?;

        let maybe_child = try_find_child(&ctx, &repo, ancestor, descendant, 100).await?;
        let child = maybe_child.ok_or_else(|| anyhow!("didn't find child"))?;
        assert_eq!(child, descendant);
        Ok(())
    }

    async fn assert_commit_cloud_writable(fb: FacebookInit, knob_on: bool) -> Result<(), Error> {
        let mut factory = test_repo_factory::TestRepoFactory::new(fb)?;
        factory.with_config_override(|config| {
            config.readonly = metaconfig_types::RepoReadOnly::ReadOnly("test".to_string());
            config.infinitepush.allow_writes = false;
        });
        let repo: Repo = factory.build().await?;
        let repo_ctx = RepoContext::new_test(CoreContext::test_mock(fb), Arc::new(repo)).await?;
        match (repo_ctx.ensure_commit_cloud_writable(), knob_on) {
            (Err(MononokeError::InvalidRequest(msg)), true)
                if msg.contains("Commit-cloud writes are disabled")
                    && msg
                        .contains("scm/mononoke:reject_commit_cloud_writes_on_readonly_repos") => {}
            (Ok(()), false) => {}
            (other, _) => panic!(
                "knob_on={knob_on}: readonly repo must be rejected iff gate on, got: {other:?}"
            ),
        }

        // Readonly but draft-accepting (the configerator shape): always allowed.
        let mut factory = test_repo_factory::TestRepoFactory::new(fb)?;
        factory.with_config_override(|config| {
            config.readonly = metaconfig_types::RepoReadOnly::ReadOnly("test".to_string());
            config.infinitepush.allow_writes = true;
        });
        let repo: Repo = factory.build().await?;
        let repo_ctx = RepoContext::new_test(CoreContext::test_mock(fb), Arc::new(repo)).await?;
        assert!(
            repo_ctx.ensure_commit_cloud_writable().is_ok(),
            "knob_on={knob_on}: readonly repo with infinitepush writes must allow commit-cloud writes"
        );

        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let repo_ctx = RepoContext::new_test(CoreContext::test_mock(fb), Arc::new(repo)).await?;
        assert!(
            repo_ctx.ensure_commit_cloud_writable().is_ok(),
            "knob_on={knob_on}: writable repo must always allow commit-cloud writes"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_commit_cloud_rejects_readonly_repo(fb: FacebookInit) -> Result<(), Error> {
        for knob_on in [true, false] {
            with_just_knobs_async(
                JustKnobsInMemory::new(HashMap::from([(
                    "scm/mononoke:reject_commit_cloud_writes_on_readonly_repos".to_string(),
                    KnobVal::Bool(knob_on),
                )])),
                assert_commit_cloud_writable(fb, knob_on).boxed(),
            )
            .await?;
        }
        Ok(())
    }
}
