/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg(fbcode_build)]
#[path = "facebook/bookmark_batches.rs"]
mod bookmark_batches;
#[cfg(fbcode_build)]
#[path = "facebook/bookmark_notifications.rs"]
mod bookmark_notifications;
#[cfg(fbcode_build)]
#[path = "facebook/event_router_batching.rs"]
mod event_router_batching;
#[cfg(fbcode_build)]
use event_router_batching::BatchSettings;
#[cfg(fbcode_build)]
#[path = "facebook/event_router_coordinator_stats.rs"]
mod event_router_coordinator_stats;
mod polling;
#[cfg(all(fbcode_build, test))]
#[path = "facebook/polling_mode_tests.rs"]
mod polling_mode_tests;
#[cfg(test)]
mod scoped_bookmarks_tests;
#[cfg(test)]
mod test_helpers;

use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::RangeBounds;
use std::sync::Arc;
#[cfg(fbcode_build)]
use std::sync::LazyLock;
use std::sync::RwLock;
#[cfg(test)]
use std::time::Duration;

#[cfg(fbcode_build)]
use MononokeWarmBookmarkCacheStats_ods3::Instrument_MononokeWarmBookmarkCacheStats;
#[cfg(fbcode_build)]
use MononokeWarmBookmarkCacheStats_ods3_types::MononokeWarmBookmarkCacheStats;
#[cfg(fbcode_build)]
use MononokeWarmBookmarkCacheStats_ods3_types::WarmBookmarkCacheEvent;
use anyhow::Context as _;
use anyhow::Error;
use anyhow::anyhow;
use async_trait::async_trait;
use basename_suffix_skeleton_manifest_v3::RootBssmV3DirectoryId;
use blame::RootBlameV2;
use blame::RootBlameV3;
use bookmarks::ArcBookmarkUpdateLog;
use bookmarks::ArcBookmarks;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogId;
#[cfg(test)]
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Bookmarks;
#[cfg(test)]
use bookmarks::BookmarksRef;
use bookmarks::BookmarksSubscription;
use bookmarks::Freshness;
use bookmarks_cache::BookmarksCache;
use bookmarks_cache::ScopedBookmarksCache;
use bookmarks_cache::WarmerRequirement;
use bookmarks_types::BookmarkKind;
use bookmarks_types::BookmarkPagination;
use bookmarks_types::BookmarkPrefix;
use case_conflict_skeleton_manifest::RootCaseConflictSkeletonManifestId;
use changeset_info::ChangesetInfo;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use context::SessionClass;
use deleted_manifest::RootDeletedManifestV2Id;
use derived_data_manager::BonsaiDerivable as NewBonsaiDerivable;
use directory_branch_cluster_manifest::RootDirectoryBranchClusterManifestId;
use enum_map::EnumMap;
use fastlog::RootFastlog;
use fastlog::RootFastlogV2;
use filenodes_derivation::FilenodesOnlyPublic;
use fsnodes::RootFsnodeId;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::future::select;
use futures::stream;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_stats::TimedFutureExt;
use futures_watchdog::WatchdogExt;
use git_types::MappedGitCommitId;
use git_types::RootGitDeltaManifestV2Id;
use history_manifest::RootHistoryManifestDirectoryId;
use inferred_copy_from::RootInferredCopyFromId;
use itertools::Itertools;
#[cfg(test)]
use lock_ext::RwLockExt;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::Timestamp;
use phases::ArcPhases;
use repo_derived_data::ArcRepoDerivedData;
use repo_event_publisher::ArcRepoEventPublisher;
#[cfg(all(fbcode_build, test))]
use repo_event_publisher::BookmarkCategory as EventRouterBookmarkCategory;
#[cfg(fbcode_build)]
use repo_event_publisher::BookmarkEventNotification;
#[cfg(fbcode_build)]
use repo_event_publisher::ParsedBookmarkEventNotification;
use repo_event_publisher::RepoEventPublisher;
use repo_identity::ArcRepoIdentity;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use skeleton_manifest::RootSkeletonManifestId;
use skeleton_manifest_v2::RootSkeletonManifestV2Id;
use stats::prelude::*;
use tokio::sync::Notify;
#[cfg(fbcode_build)]
use tokio::sync::broadcast;
use tracing::Instrument;
use unodes::RootUnodeManifestId;

mod warmers;
pub use warmers::create_derived_data_warmer;
pub use warmers::create_public_phase_warmer;

/// The coordinator notifies `complete` after servicing a `start` request.
struct SyncSignals {
    start: Arc<Notify>,
    complete: Arc<Notify>,
}

#[cfg(fbcode_build)]
static WBC_INSTRUMENT: LazyLock<Instrument_MononokeWarmBookmarkCacheStats> =
    LazyLock::new(Instrument_MononokeWarmBookmarkCacheStats::new);

define_stats! {
    prefix = "mononoke.warm_bookmarks_cache";
    max_staleness_secs: dynamic_singleton_counter("{}.max_staleness_secs", (reponame: String)),
    event_router_coordinator_active: dynamic_singleton_counter(
        "{}.event_router_coordinator_active",
        (reponame: String)
    ),
    event_router_notification_processed_count: dynamic_timeseries(
        "{}.event_router_notification_processed_count",
        (reponame: String);
        Average, Sum, Count
    ),
    event_router_notification_failure_count: dynamic_timeseries(
        "{}.event_router_notification_failure_count",
        (reponame: String);
        Average, Sum, Count
    ),
    event_router_subscription_failure_count: dynamic_timeseries(
        "{}.event_router_subscription_failure_count.{}",
        (reponame: String, reason: String);
        Average, Sum, Count
    ),
    event_router_subscriber_lag_count: dynamic_timeseries(
        "{}.event_router_subscriber_lag_count",
        (reponame: String);
        Average, Sum, Count
    ),
    event_router_full_refresh_count: dynamic_timeseries(
        "{}.event_router_full_refresh_count.{}",
        (reponame: String, reason: String);
        Average, Sum, Count
    ),
    global_max_staleness_secs: histogram(10, 0, 5000, Average; P 50; P 75; P 95; P 99),
}

/// Represents the state of derived data for a particular warmer requirement.
///
/// Each bookmark can track derived data for different requirements (HgOnly, GitOnly, AllKinds).
/// This enum captures whether a requirement is being tracked, and if so, what changeset
/// the derived data is currently at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementState {
    /// This requirement is not being tracked for this bookmark.
    ///
    /// This state occurs when no warmers with the corresponding tag (Hg or Git) are configured.
    /// Attempting to get or set a NotTracked requirement will return an error.
    NotTracked,

    /// The requirement is being tracked, but the derived data changeset is not yet known.
    ///
    /// This is the initial state for tracked requirements before they are warmed or initialized.
    /// When in this state, `get()` will return `Ok(None)`, indicating the requirement is tracked
    /// but not yet derived.
    Unknown,

    /// The requirement is tracked and the derived data is at the specified changeset.
    ///
    /// This means all warmers for this requirement have successfully derived data up to
    /// (and including) the given changeset ID.
    At(ChangesetId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkState {
    kind: BookmarkKind,
    log_id: Option<BookmarkUpdateLogId>,
    derived_cs_per_requirement: EnumMap<WarmerRequirement, RequirementState>,
}

impl BookmarkState {
    pub fn from_warmers(kind: BookmarkKind, warmers: &[Warmer]) -> Self {
        let mut derived_cs_per_requirement = enum_map::enum_map! {
            WarmerRequirement::HgOnly => RequirementState::NotTracked,
            WarmerRequirement::GitOnly => RequirementState::NotTracked,
            WarmerRequirement::AllKinds => RequirementState::Unknown,
        };

        for warmer in warmers {
            for tag in &warmer.tags {
                match tag {
                    WarmerTag::Hg => {
                        derived_cs_per_requirement[WarmerRequirement::HgOnly] =
                            RequirementState::Unknown
                    }
                    WarmerTag::Git => {
                        derived_cs_per_requirement[WarmerRequirement::GitOnly] =
                            RequirementState::Unknown
                    }
                }
            }
        }

        Self {
            kind,
            log_id: None,
            derived_cs_per_requirement,
        }
    }

    /// Returns the bookmark update log ID represented by this cached state.
    pub fn log_id(&self) -> Option<BookmarkUpdateLogId> {
        self.log_id
    }

    fn set_log_id(&mut self, log_id: BookmarkUpdateLogId) {
        self.log_id = Some(log_id);
    }

    pub fn set_all_tracked(&mut self, cs_id: ChangesetId) {
        for req in [
            WarmerRequirement::HgOnly,
            WarmerRequirement::GitOnly,
            WarmerRequirement::AllKinds,
        ] {
            if self.derived_cs_per_requirement[req] != RequirementState::NotTracked {
                self.set(req, cs_id)
                    .expect("Tried to set an untracked requirement, even after checking!");
            }
        }
    }

    /// Sets multiple requirements to the given changeset ID, if they are tracked
    /// in this state instance
    pub fn set_many(
        &mut self,
        requirements: &[WarmerRequirement],
        cs_id: ChangesetId,
    ) -> Result<(), Error> {
        for req in requirements {
            self.set(*req, cs_id)?;
        }
        Ok(())
    }

    /// Sets a single requirement to the given changeset ID, if it's tracked in
    /// this state instance
    pub fn set(&mut self, requirement: WarmerRequirement, cs_id: ChangesetId) -> Result<(), Error> {
        if self.derived_cs_per_requirement[requirement] != RequirementState::NotTracked {
            self.derived_cs_per_requirement[requirement] = RequirementState::At(cs_id);
            Ok(())
        } else {
            Err(anyhow!(
                "The specified requirement is not being tracked: {requirement:?}"
            ))
        }
    }

    /// Gets the changeset ID for a requirement, returning None if NotTracked or Unknown.
    pub fn get(&self, requirement: WarmerRequirement) -> Result<Option<ChangesetId>, Error> {
        match self.derived_cs_per_requirement[requirement] {
            RequirementState::At(cs_id) => Ok(Some(cs_id)),
            RequirementState::Unknown => Ok(None),
            RequirementState::NotTracked => Err(anyhow!(
                "The requirement requested is not being tracked: {requirement:?}"
            )),
        }
    }
}

pub struct WarmBookmarksCache {
    bookmarks: Arc<RwLock<HashMap<BookmarkKey, BookmarkState>>>,
    terminate: Option<oneshot::Sender<()>>,
    notify_sync_start: Arc<Notify>,
    notify_sync_complete: Arc<Notify>,
    default_warmer_requirement: WarmerRequirement,
}

pub type WarmerFn =
    dyn for<'a> Fn(&'a CoreContext, ChangesetId) -> BoxFuture<'a, Result<(), Error>> + Send + Sync;

pub type IsWarmFn = dyn for<'a> Fn(&'a CoreContext, ChangesetId) -> BoxFuture<'a, Result<bool, Error>>
    + Send
    + Sync;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, enum_map::Enum)]
pub enum WarmerTag {
    Hg,
    Git,
}

pub struct Warmer {
    warmer: Box<WarmerFn>,
    is_warm: Box<IsWarmFn>,
    name: String,
    #[allow(unused)]
    tags: Vec<WarmerTag>,
}

/// Initialization mode for the warm bookmarks cache.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InitMode {
    /// Rewind each bookmark until a warm changeset is found.
    Rewind,

    /// Warm up the bookmark at its current location.
    Warm,
}

pub struct WarmBookmarksCacheBuilder {
    ctx: CoreContext,
    bookmarks: ArcBookmarks,
    bookmark_update_log: ArcBookmarkUpdateLog,
    repo_identity: ArcRepoIdentity,
    repo_event_publisher: ArcRepoEventPublisher,
    #[cfg(fbcode_build)]
    event_router_configured: bool,
    warmers: Vec<Warmer>,
    init_mode: InitMode,
    default_warmer_requirement: WarmerRequirement,
}

impl WarmBookmarksCacheBuilder {
    pub fn new(
        mut ctx: CoreContext,
        bookmarks: ArcBookmarks,
        bookmark_update_log: ArcBookmarkUpdateLog,
        repo_identity: ArcRepoIdentity,
        repo_event_publisher: ArcRepoEventPublisher,
        warmer_requirement: WarmerRequirement,
    ) -> Self {
        ctx.session_mut()
            .override_session_class(SessionClass::WarmBookmarksCache);
        let ctx = ctx.with_mutated_scuba(|mut scuba_sample_builder| {
            scuba_sample_builder.add("repo", repo_identity.name());
            scuba_sample_builder.add_common_server_data();
            scuba_sample_builder
        });

        Self {
            ctx,
            bookmarks,
            bookmark_update_log,
            repo_identity,
            repo_event_publisher,
            #[cfg(fbcode_build)]
            event_router_configured: false,
            warmers: vec![],
            init_mode: InitMode::Rewind,
            default_warmer_requirement: warmer_requirement,
        }
    }

    /// Whether a typed EventRouter notification category is configured.
    #[cfg(fbcode_build)]
    pub fn set_event_router_configured(&mut self, configured: bool) {
        self.event_router_configured = configured;
    }

    pub fn add_all_warmers(
        &mut self,
        repo_derived_data: &ArcRepoDerivedData,
        phases: &ArcPhases,
    ) -> Result<(), Error> {
        self.add_derived_data_warmers(&repo_derived_data.active_config().types, repo_derived_data)?;
        self.add_public_phase_warmer(phases);
        Ok(())
    }

    pub fn add_hg_warmers(
        &mut self,
        repo_derived_data: &ArcRepoDerivedData,
        phases: &ArcPhases,
    ) -> Result<(), Error> {
        self.add_derived_data_warmers(
            &[
                MappedHgChangesetId::VARIANT,
                FilenodesOnlyPublic::VARIANT,
                RootHgAugmentedManifestId::VARIANT,
            ],
            repo_derived_data,
        )?;
        self.add_public_phase_warmer(phases);
        Ok(())
    }

    pub fn add_git_warmers(
        &mut self,
        repo_derived_data: &ArcRepoDerivedData,
        phases: &ArcPhases,
    ) -> Result<(), Error> {
        self.add_derived_data_warmers(
            &[
                MappedGitCommitId::VARIANT,
                RootGitDeltaManifestV2Id::VARIANT,
            ],
            repo_derived_data,
        )?;
        self.add_public_phase_warmer(phases);
        Ok(())
    }

    pub fn add_specific_types_warmers(
        &mut self,
        repo_derived_data: &ArcRepoDerivedData,
        types: &[DerivableType],
        phases: &ArcPhases,
    ) -> Result<(), Error> {
        self.add_derived_data_warmers(types, repo_derived_data)?;
        self.add_public_phase_warmer(phases);
        Ok(())
    }

    fn add_derived_data_warmers<'a>(
        &mut self,
        types: impl IntoIterator<Item = &'a DerivableType>,
        repo_derived_data: &ArcRepoDerivedData,
    ) -> Result<(), Error> {
        let types = types.into_iter().collect::<HashSet<_>>();

        let config = repo_derived_data.config();
        for ty in types.iter() {
            if config.is_enabled(**ty) && !config.is_excluded_from_wbc(**ty) {
                self.warmers
                    .extend(self.derived_data_warmer(ty, repo_derived_data));
            }
        }

        Ok(())
    }

    fn derived_data_warmer(
        &self,
        derivable_type: &DerivableType,
        repo_derived_data: &ArcRepoDerivedData,
    ) -> Option<Warmer> {
        match derivable_type {
            DerivableType::Unodes => Some(create_derived_data_warmer::<RootUnodeManifestId>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::BlameV2 => Some(create_derived_data_warmer::<RootBlameV2>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::BlameV3 => Some(create_derived_data_warmer::<RootBlameV3>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::FileNodes => Some(create_derived_data_warmer::<FilenodesOnlyPublic>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg],
            )),
            DerivableType::HgChangesets => Some(create_derived_data_warmer::<MappedHgChangesetId>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg],
            )),
            DerivableType::HgAugmentedManifests => {
                Some(create_derived_data_warmer::<RootHgAugmentedManifestId>(
                    &self.ctx,
                    repo_derived_data.clone(),
                    vec![WarmerTag::Hg],
                ))
            }
            DerivableType::Fsnodes => Some(create_derived_data_warmer::<RootFsnodeId>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::Fastlog => Some(create_derived_data_warmer::<RootFastlog>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::FastlogV2 => Some(create_derived_data_warmer::<RootFastlogV2>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::DeletedManifests => {
                Some(create_derived_data_warmer::<RootDeletedManifestV2Id>(
                    &self.ctx,
                    repo_derived_data.clone(),
                    vec![WarmerTag::Hg, WarmerTag::Git],
                ))
            }
            DerivableType::SkeletonManifests => {
                Some(create_derived_data_warmer::<RootSkeletonManifestId>(
                    &self.ctx,
                    repo_derived_data.clone(),
                    vec![WarmerTag::Hg, WarmerTag::Git],
                ))
            }
            DerivableType::SkeletonManifestsV2 => {
                Some(create_derived_data_warmer::<RootSkeletonManifestV2Id>(
                    &self.ctx,
                    repo_derived_data.clone(),
                    vec![WarmerTag::Hg, WarmerTag::Git],
                ))
            }
            DerivableType::Ccsm => Some(create_derived_data_warmer::<
                RootCaseConflictSkeletonManifestId,
            >(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::ContentManifests => {
                Some(create_derived_data_warmer::<RootContentManifestId>(
                    &self.ctx,
                    repo_derived_data.clone(),
                    vec![WarmerTag::Hg, WarmerTag::Git],
                ))
            }
            DerivableType::ChangesetInfo => Some(create_derived_data_warmer::<ChangesetInfo>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::GitDeltaManifestsV2 => {
                Some(create_derived_data_warmer::<RootGitDeltaManifestV2Id>(
                    &self.ctx,
                    repo_derived_data.clone(),
                    vec![WarmerTag::Git],
                ))
            }
            DerivableType::GitDeltaManifestsV3 => None,
            DerivableType::BssmV3 => Some(create_derived_data_warmer::<RootBssmV3DirectoryId>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
            DerivableType::GitCommits => Some(create_derived_data_warmer::<MappedGitCommitId>(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Git],
            )),
            DerivableType::InferredCopyFrom => {
                Some(create_derived_data_warmer::<RootInferredCopyFromId>(
                    &self.ctx,
                    repo_derived_data.clone(),
                    vec![WarmerTag::Hg, WarmerTag::Git],
                ))
            }
            DerivableType::DirectoryBranchClusterManifest => {
                Some(create_derived_data_warmer::<
                    RootDirectoryBranchClusterManifestId,
                >(
                    &self.ctx, repo_derived_data.clone(), vec![WarmerTag::Hg]
                ))
            }
            DerivableType::AclManifests => None,
            DerivableType::TestManifests => None,
            DerivableType::TestShardedManifests => None,
            DerivableType::HistoryManifests => Some(create_derived_data_warmer::<
                RootHistoryManifestDirectoryId,
            >(
                &self.ctx,
                repo_derived_data.clone(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            )),
        }
    }

    fn add_public_phase_warmer(&mut self, phases: &ArcPhases) {
        let warmer = create_public_phase_warmer(&self.ctx, phases.clone());
        self.warmers.push(warmer);
    }

    /// For use in tests to avoid having to wait for the warm bookmark cache
    /// to finish its initial warming cycle.  When initializing the warm
    /// bookmarks cache, wait until all current bookmark values have been
    /// warmed before returning.
    pub fn wait_until_warmed(&mut self) {
        self.init_mode = InitMode::Warm;
    }

    pub async fn build(self) -> Result<WarmBookmarksCache, Error> {
        WarmBookmarksCache::from_builder(self).await
    }
}

/// A drop-in replacement for warm bookmark cache that doesn't
/// cache anything but goes to bookmarks struct every time.
/// Implements both ScopedBookmarksCache and BookmarksCache traits,
/// ignoring the WarmerRequirement scope parameter.
pub struct NoopBookmarksCache {
    bookmarks: ArcBookmarks,
}

impl NoopBookmarksCache {
    pub fn new(bookmarks: ArcBookmarks) -> Self {
        NoopBookmarksCache { bookmarks }
    }
}

#[async_trait]
impl ScopedBookmarksCache for NoopBookmarksCache {
    async fn get(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkKey,
        _scope: WarmerRequirement,
    ) -> Result<Option<ChangesetId>, Error> {
        self.bookmarks
            .get(ctx.clone(), bookmark, bookmarks::Freshness::MostRecent)
            .await
    }

    async fn list(
        &self,
        ctx: &CoreContext,
        prefix: &BookmarkPrefix,
        pagination: &BookmarkPagination,
        limit: Option<u64>,
        _scope: WarmerRequirement,
    ) -> Result<Vec<(BookmarkKey, (ChangesetId, BookmarkKind))>, Error> {
        self.bookmarks
            .list(
                ctx.clone(),
                Freshness::MaybeStale,
                prefix,
                BookmarkCategory::ALL,
                BookmarkKind::ALL_PUBLISHING,
                pagination,
                limit.unwrap_or(u64::MAX),
            )
            .map_ok(|(book, cs_id)| {
                let kind = *book.kind();
                (book.into_key(), (cs_id, kind))
            })
            .try_collect()
            .await
    }

    async fn sync(&self, _ctx: &CoreContext) {}
}

/// Implements BookmarksCache by delegating to ScopedBookmarksCache with AllKinds scope.
/// NoopBookmarksCache doesn't do any caching or warming, so the scope doesn't matter.
#[async_trait]
impl BookmarksCache for NoopBookmarksCache {
    async fn get(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkKey,
    ) -> Result<Option<ChangesetId>, Error> {
        ScopedBookmarksCache::get(self, ctx, bookmark, WarmerRequirement::AllKinds).await
    }

    async fn list(
        &self,
        ctx: &CoreContext,
        prefix: &BookmarkPrefix,
        pagination: &BookmarkPagination,
        limit: Option<u64>,
    ) -> Result<Vec<(BookmarkKey, (ChangesetId, BookmarkKind))>, Error> {
        ScopedBookmarksCache::list(
            self,
            ctx,
            prefix,
            pagination,
            limit,
            WarmerRequirement::AllKinds,
        )
        .await
    }

    async fn sync(&self, ctx: &CoreContext) {
        ScopedBookmarksCache::sync(self, ctx).await;
    }
}

impl WarmBookmarksCache {
    pub async fn new(
        ctx: &CoreContext,
        bookmarks: &ArcBookmarks,
        bookmark_update_log: &ArcBookmarkUpdateLog,
        repo_identity: &ArcRepoIdentity,
        repo_event_publisher: &ArcRepoEventPublisher,
        warmers: Vec<Warmer>,
        init_mode: InitMode,
        default_warmer_requirement: WarmerRequirement,
    ) -> Result<Self, Error> {
        WarmBookmarksCacheBuilder {
            ctx: ctx.clone(),
            bookmarks: bookmarks.clone(),
            bookmark_update_log: bookmark_update_log.clone(),
            repo_identity: repo_identity.clone(),
            repo_event_publisher: repo_event_publisher.clone(),
            #[cfg(fbcode_build)]
            event_router_configured: false,
            warmers,
            init_mode,
            default_warmer_requirement,
        }
        .build()
        .await
    }

    async fn from_builder(builder: WarmBookmarksCacheBuilder) -> Result<Self, Error> {
        let WarmBookmarksCacheBuilder {
            ctx,
            bookmarks,
            bookmark_update_log,
            repo_identity,
            repo_event_publisher,
            #[cfg(fbcode_build)]
            event_router_configured,
            warmers,
            init_mode,
            default_warmer_requirement,
        } = builder;
        let ctx = &ctx;
        let warmers = Arc::new(warmers);
        let (sender, receiver) = oneshot::channel();
        let notify_sync_start = Arc::new(Notify::new());
        let notify_sync_complete = Arc::new(Notify::new());

        let warmer_names = warmers.iter().map(|w| w.name.as_str()).join(", ");
        tracing::info!(
            repo = %repo_identity.name(),
            warmers = %warmer_names,
            "Starting warm bookmark cache updater"
        );
        tracing::info!(
            repo = %repo_identity.name(),
            "WBC: creating bookmarks subscription"
        );
        let sub = bookmarks
            .create_subscription(ctx, Freshness::MaybeStale)
            .await
            .context("Error creating bookmarks subscription")?;
        tracing::info!(
            repo = %repo_identity.name(),
            "WBC: bookmarks subscription created, starting init_bookmarks"
        );

        let bookmarks_to_watch = init_bookmarks(
            ctx,
            &*sub,
            bookmarks.as_ref(),
            bookmark_update_log.as_ref(),
            &warmers,
            init_mode,
        )
        .instrument(tracing::info_span!("init bookmarks", repo = %repo_identity.name()))
        .await?;
        tracing::info!(
            repo = %repo_identity.name(),
            bookmark_count = bookmarks_to_watch.len(),
            "WBC: init_bookmarks completed, spawning coordinator"
        );

        let bookmarks_to_watch = Arc::new(RwLock::new(bookmarks_to_watch));

        let coordinator = BookmarksCoordinator::new(
            bookmarks_to_watch.clone(),
            bookmarks.clone(),
            bookmark_update_log.clone(),
            repo_identity.clone(),
            repo_event_publisher.clone(),
            warmers.clone(),
        );
        let sync = SyncSignals {
            start: notify_sync_start.clone(),
            complete: notify_sync_complete.clone(),
        };
        #[cfg(fbcode_build)]
        if event_router_configured {
            match repo_event_publisher
                .subscribe_for_bookmark_event_notifications(&repo_identity.name().to_owned())
            {
                Ok(subscriber) => {
                    coordinator.spawn_event_router(ctx.clone(), receiver, sync, Some(subscriber))
                }
                Err(error) => {
                    STATS::event_router_subscription_failure_count
                        .add_value(1, (repo_identity.name().to_owned(), "subscribe".to_owned()));
                    tracing::warn!(
                        repo = %repo_identity.name(),
                        ?error,
                        "failed to subscribe to EventRouter bookmark notifications; the coordinator retries"
                    );
                    coordinator.spawn_event_router(ctx.clone(), receiver, sync, None)
                }
            }
        } else {
            coordinator.spawn_polling(ctx.clone(), receiver, sync);
        }
        #[cfg(not(fbcode_build))]
        coordinator.spawn_polling(ctx.clone(), receiver, sync);
        tracing::info!(
            repo = %repo_identity.name(),
            "WBC: coordinator spawned, warm bookmarks cache ready"
        );

        Ok(Self {
            bookmarks: bookmarks_to_watch,
            terminate: Some(sender),
            notify_sync_start,
            notify_sync_complete,
            default_warmer_requirement,
        })
    }
}

#[async_trait]
impl ScopedBookmarksCache for WarmBookmarksCache {
    async fn get(
        &self,
        _ctx: &CoreContext,
        bookmark: &BookmarkKey,
        scope: WarmerRequirement,
    ) -> Result<Option<ChangesetId>, Error> {
        Ok(self
            .bookmarks
            .read()
            .map_err(|e| anyhow!("Failed to take bookmarks lock: {e:#?}"))?
            .get(bookmark)
            .map(|state| state.get(scope))
            .transpose()?
            .flatten())
    }

    async fn list(
        &self,
        _ctx: &CoreContext,
        prefix: &BookmarkPrefix,
        pagination: &BookmarkPagination,
        limit: Option<u64>,
        scope: WarmerRequirement,
    ) -> Result<Vec<(BookmarkKey, (ChangesetId, BookmarkKind))>, Error> {
        let bookmarks = self
            .bookmarks
            .read()
            .map_err(|e| anyhow!("Failed to take bookmarks lock: {e:#?}"))?;

        if prefix.is_empty() && *pagination == BookmarkPagination::FromStart && limit.is_none() {
            // Simple case: return all bookmarks
            bookmarks
                .iter()
                .filter_map(|(key, state)| match state.get(scope) {
                    Ok(Some(cs_id)) => Some(Ok((key.clone(), (cs_id, state.kind)))),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect()
        } else {
            // Filter based on prefix and pagination
            let range = prefix.to_range().with_pagination(pagination.clone());
            let mut matches = bookmarks
                .iter()
                .filter(|(key, _)| range.contains(key))
                .filter_map(|(key, state)| match state.get(scope) {
                    Ok(Some(cs_id)) => Some(Ok((key.clone(), (cs_id, state.kind)))),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Release the read lock.
            drop(bookmarks);
            if let Some(limit) = limit {
                // We must sort and truncate if there is a limit so that the
                // client can paginate in order.
                matches.sort_by(|(key1, _): &(BookmarkKey, _), (key2, _)| key1.cmp(key2));
                matches.truncate(limit as usize);
            }
            Ok(matches)
        }
    }

    async fn sync(&self, _ctx: &CoreContext) {
        // Notifies the bookmark coordinator of sync start and
        // starts listening for a notification indicating sync completion.
        let notified = self.notify_sync_complete.notified();
        self.notify_sync_start.notify_one();
        notified.await;
    }
}

/// Implements BookmarksCache by delegating to ScopedBookmarksCache with the configured default_warmer_requirement.
/// This allows different services (Hg, Git, SCS) to use different scopes without passing the requirement
/// through every API call.
#[async_trait]
impl BookmarksCache for WarmBookmarksCache {
    async fn get(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkKey,
    ) -> Result<Option<ChangesetId>, Error> {
        // Use the stored default_warmer_requirement instead of always using AllKinds
        ScopedBookmarksCache::get(self, ctx, bookmark, self.default_warmer_requirement).await
    }

    async fn list(
        &self,
        ctx: &CoreContext,
        prefix: &BookmarkPrefix,
        pagination: &BookmarkPagination,
        limit: Option<u64>,
    ) -> Result<Vec<(BookmarkKey, (ChangesetId, BookmarkKind))>, Error> {
        // Use the stored default_warmer_requirement instead of always using AllKinds
        ScopedBookmarksCache::list(
            self,
            ctx,
            prefix,
            pagination,
            limit,
            self.default_warmer_requirement,
        )
        .await
    }

    async fn sync(&self, ctx: &CoreContext) {
        ScopedBookmarksCache::sync(self, ctx).await;
    }
}

impl Drop for WarmBookmarksCache {
    fn drop(&mut self) {
        // Ignore any error - we don't care if the updater has gone away.
        if let Some(terminate) = self.terminate.take() {
            let _ = terminate.send(());
        }
    }
}

async fn init_bookmarks(
    ctx: &CoreContext,
    sub: &dyn BookmarksSubscription,
    bookmarks: &dyn Bookmarks,
    bookmark_update_log: &dyn BookmarkUpdateLog,
    warmers: &Arc<Vec<Warmer>>,
    mode: InitMode,
) -> Result<HashMap<BookmarkKey, BookmarkState>, Error> {
    let all_bookmarks = sub.bookmarks();
    let total = all_bookmarks.len();

    tracing::info!("{} bookmarks to warm up", total);

    let warm_cs_ids = warm_changesets(
        ctx,
        all_bookmarks.values().map(|(cs_id, _, _)| *cs_id),
        warmers,
    )
    .await;
    let warm_cs_ids = &warm_cs_ids;

    let futs = all_bookmarks
        .iter()
        .enumerate()
        .map(|(i, (book, (cs_id, kind, log_id)))| async move {
            let book = book.clone();
            let kind = *kind;
            let cs_id = *cs_id;
            let log_id = *log_id;

            let remaining = total - i - 1;

            if !warm_cs_ids.contains(&cs_id) {
                match mode {
                    InitMode::Rewind => {
                        let derived_cs_ids = move_bookmark_back_in_history_until_derived(
                            ctx,
                            bookmarks,
                            bookmark_update_log,
                            &book,
                            warmers,
                        )
                        .watched()
                        .await?;

                        for (requirement, maybe_cs_id) in derived_cs_ids.changesets {
                            if let Some(cs_id) = maybe_cs_id {
                                tracing::info!(
                                    "moved {} back in {:?} history to {:?}",
                                    book,
                                    requirement,
                                    cs_id
                                );
                            }
                        }

                        // Insert bookmark if ANY scope is ready (not just AllKinds).
                        // This allows bookmarks to become visible with partial warming.
                        let has_any_scope = derived_cs_ids.changesets.values().any(|v| v.is_some());

                        let key_and_state_pair = if has_any_scope {
                            // Create BookmarkState from warmers to track the right requirements
                            let mut state = BookmarkState::from_warmers(kind, warmers);

                            // Set each requirement that we found a derived version for
                            for (requirement, maybe_cs_id) in derived_cs_ids.changesets {
                                if let Some(cs_id) = maybe_cs_id {
                                    state.set(requirement, cs_id)?;
                                }
                            }

                            Some((book, state))
                        } else {
                            None
                        };

                        Ok((remaining, key_and_state_pair))
                    }
                    InitMode::Warm => {
                        tracing::info!("warmed bookmark {} at {}", book, cs_id);
                        warm_all(ctx, cs_id, warmers).watched().await?;
                        let mut bookmarks_state = BookmarkState::from_warmers(kind, warmers);
                        bookmarks_state.set_all_tracked(cs_id);
                        if let Some(log_id) = log_id {
                            bookmarks_state.set_log_id(log_id);
                        }
                        Ok((remaining, Some((book, bookmarks_state))))
                    }
                }
            } else {
                let mut bookmarks_state = BookmarkState::from_warmers(kind, warmers);
                bookmarks_state.set_all_tracked(cs_id);
                if let Some(log_id) = log_id {
                    bookmarks_state.set_log_id(log_id);
                }
                Ok((remaining, Some((book, bookmarks_state))))
            }
        })
        .collect::<Vec<_>>(); // This should be unnecessary but it de-confuses the compiler: P415515287.

    let res = stream::iter(futs)
        .buffered(100)
        .try_filter_map(|element| async move {
            let (remaining, entry) = element;
            if remaining % 1000 == 0 {
                tracing::info!("{} bookmarks left to warm up", remaining);
            }
            Result::<_, Error>::Ok(entry)
        })
        .try_collect::<HashMap<_, _>>()
        .await
        .with_context(|| "Error warming up bookmarks")?;

    tracing::info!("all bookmarks are warmed up");

    Ok(res)
}

/// Status of derived data for a specific tag (Hg or Git).
#[derive(PartialEq, Eq, Default, Copy, Clone)]
enum TagStatus {
    /// Derivation status has not been checked yet.
    Unknown,
    /// All derived data for this tag is available.
    Warm,
    /// At least one piece of derived data for this tag is missing.
    Cold,
    /// This tag is not being tracked
    #[default]
    Untracked,
}

/// Tracks the warm/cold status of derived data across different warmer tags.
///
/// This struct maintains the derivation status for each one of the tags in the configured warmers.
/// Tags not tracked by any warmer are represented as `None` and are ignored.
///
/// The status follows AND semantics: once a tag becomes Cold, it stays Cold until
/// re-initialized, ensuring conservative tracking of derived data availability.
struct WarmState {
    state: EnumMap<WarmerTag, TagStatus>,
}

impl WarmState {
    /// Creates a new WarmState initialized with tags from the provided warmers.
    ///
    /// Only tags present in at least one warmer will be tracked (set to `Unknown`).
    /// Other tags remain `None` and are not tracked.
    ///
    /// # Arguments
    /// * `warmers` - Slice of warmers whose tags should be tracked
    ///
    /// # Returns
    /// A new WarmState with all tracked tags initialized to `Unknown`
    fn new(warmers: &[Warmer]) -> Self {
        let mut state = EnumMap::default(); // All Untracked by default
        let tracked_tags: HashSet<WarmerTag> = warmers
            .iter()
            .flat_map(|w| w.tags.iter().copied())
            .collect();
        for tag in tracked_tags {
            state[tag] = TagStatus::Unknown;
        }
        WarmState { state }
    }

    /// Creates an empty WarmState with no tracked tags.
    ///
    /// This is used for cases where there's no changeset (e.g., deleted bookmarks).
    /// Since there's no derived data to compute, `are_all_warm()` will return `true`
    /// immediately (vacuous truth over empty set).
    ///
    /// # Returns
    /// A new WarmState with no tracked tags (all `None`)
    fn empty() -> Self {
        WarmState {
            state: EnumMap::default(),
        }
    }

    /// Checks if a specific tag is warm (all derived data available).
    ///
    /// # Arguments
    /// * `tag` - The warmer tag to check
    ///
    /// # Returns
    /// `true` if the tag is being tracked and is in the Warm state, `false` otherwise
    fn is_warm(&self, tag: WarmerTag) -> bool {
        self.state[tag] == TagStatus::Warm
    }

    /// Checks if all tracked tags are warm.
    ///
    /// Only considers tags that are being tracked (not Untracked).
    /// Returns `true` if all tracked tags are `Warm`, or if no tags are being tracked.
    ///
    /// # Returns
    /// `true` if all tracked tags are Warm, `false` if any tracked tag is Unknown or Cold
    fn are_all_warm(&self) -> bool {
        self.state
            .iter()
            .filter(|&(_, &val)| val != TagStatus::Untracked)
            .all(|(_, val)| *val == TagStatus::Warm)
    }

    /// Updates the warm/cold status of a tag based on a check result.
    ///
    /// Implements AND semantics for combining multiple warmer results:
    /// - `Unknown` or `Warm` → `Warm` if `is_warm` is true, `Cold` otherwise
    /// - `Cold` → stays `Cold` (pessimistic AND logic)
    ///
    /// If called on a non-tracked tag, logs an error and returns without updating.
    ///
    /// # Arguments
    /// * `tag` - The warmer tag to update
    /// * `is_warm` - Whether this particular check found the data to be warm
    fn apply(&mut self, tag: WarmerTag, is_warm: bool) -> Result<(), Error> {
        self.state[tag] = match self.state[tag] {
            TagStatus::Cold => TagStatus::Cold, // Once cold, stay cold (AND logic)
            TagStatus::Unknown | TagStatus::Warm => {
                if is_warm {
                    TagStatus::Warm
                } else {
                    TagStatus::Cold
                }
            }
            TagStatus::Untracked => {
                return Err(anyhow!(
                    "WarmState.apply() called on non-tracked tag {tag:?}"
                ));
            }
        };
        Ok(())
    }
}

/// Returns the changesets that are warm for every warmer. Each distinct
/// changeset is checked once, however many bookmarks point at it. A changeset
/// whose check fails is treated as cold, so its bookmarks are walked back
/// instead of failing the whole initialization.
async fn warm_changesets(
    ctx: &CoreContext,
    cs_ids: impl IntoIterator<Item = ChangesetId>,
    warmers: &[Warmer],
) -> HashSet<ChangesetId> {
    let distinct: HashSet<ChangesetId> = cs_ids.into_iter().collect();
    tracing::info!("{} distinct changesets to check", distinct.len());

    stream::iter(distinct)
        .map(|cs_id| async move {
            match get_warm_state(ctx, cs_id, warmers).watched().await {
                Ok(warm_state) => warm_state.are_all_warm().then_some(cs_id),
                Err(err) => {
                    tracing::warn!(
                        "warm check for {} failed, treating it as cold: {:#}",
                        cs_id,
                        err
                    );
                    None
                }
            }
        })
        .buffer_unordered(100)
        .filter_map(|cs_id| async move { cs_id })
        .collect()
        .await
}

async fn get_warm_state(
    ctx: &CoreContext,
    cs_id: ChangesetId,
    warmers: &[Warmer],
) -> Result<WarmState, Error> {
    if warmers.is_empty() {
        tracing::warn!(
            "get_warm_state called with empty warmers list for changeset {:?}. This likely indicates a misconfiguration.",
            cs_id
        );
    }

    warmers
        .iter()
        .map(|warmer| async move {
            let warm = (*warmer.is_warm)(ctx, cs_id).await.unwrap_or(false);
            Ok((warm, &warmer.tags))
        })
        .collect::<FuturesUnordered<_>>()
        .try_fold(
            WarmState::new(warmers),
            |mut state, (warm, tags)| async move {
                for tag in tags {
                    state.apply(*tag, warm)?;
                }
                Ok(state)
            },
        )
        .await
}

async fn warm_all(ctx: &CoreContext, cs_id: ChangesetId, warmers: &[Warmer]) -> Result<(), Error> {
    stream::iter(warmers.iter().map(Ok))
        .try_for_each_concurrent(100, |warmer| async {
            let (stats, res) = (*warmer.warmer)(ctx, cs_id).timed().await;
            let mut scuba = ctx.scuba().clone();
            scuba
                .add("Warmer name", warmer.name.clone())
                .add_future_stats(&stats);
            match &res {
                Ok(()) => {
                    scuba.log_with_msg("Warmer succeed", None);
                }
                Err(err) => {
                    scuba.log_with_msg("Warmer failed", Some(format!("{err:#}")));
                }
            }
            res
        })
        .await
}

pub struct DerivedChangesetIds {
    pub changesets: EnumMap<WarmerRequirement, Option<ChangesetId>>,
}

async fn move_bookmark_back_in_history_until_derived(
    ctx: &CoreContext,
    bookmarks: &dyn Bookmarks,
    bookmark_update_log: &dyn BookmarkUpdateLog,
    book: &BookmarkKey,
    warmers: &Arc<Vec<Warmer>>,
) -> Result<DerivedChangesetIds, Error> {
    tracing::info!("moving {} bookmark back in history...", book);

    let entries =
        find_latest_derived_and_underived(ctx, bookmarks, bookmark_update_log, book, warmers)
            .await?;

    let mut derived_changesets = EnumMap::default();
    let mut cur_bookmark_value = None;
    let mut tried = false;
    for (requirement, derived_entry) in entries.derived_entries {
        let cs_id = match derived_entry {
            LatestDerivedBookmarkEntry::Found(maybe_cs_id_and_ts) => {
                maybe_cs_id_and_ts.map(|(cs_id, _)| cs_id)
            }
            LatestDerivedBookmarkEntry::NotFound => {
                if !tried && cur_bookmark_value.is_none() {
                    tried = true;
                    cur_bookmark_value = bookmarks
                        .get(ctx.clone(), book, bookmarks::Freshness::MostRecent)
                        .await?;
                }
                tracing::warn!(
                    "cannot find previous derived version of {} for {:?} warmers, returning current version {:?}",
                    book,
                    requirement,
                    cur_bookmark_value
                );
                cur_bookmark_value
            }
        };
        derived_changesets[requirement] = cs_id;
    }

    Ok(DerivedChangesetIds {
        changesets: derived_changesets,
    })
}

#[derive(Clone, Copy)]
pub enum LatestDerivedBookmarkEntry {
    /// Timestamp can be None if no history entries are found
    Found(Option<(ChangesetId, Option<Timestamp>)>),
    /// Latest derived bookmark entry is too far away
    NotFound,
}

#[derive(Default)]
pub struct LatestUnderivedBookmarkEntry {
    /// Changeset ID of the latest underived bookmark entry.  This is the next
    /// thing to try to derive.
    maybe_cs_id: Option<ChangesetId>,
    /// ID and TS for the oldest underived bookmark entry for logging
    maybe_id_ts: Option<(BookmarkUpdateLogId, Timestamp)>,
}

pub struct LatestBookmarkEntries {
    pub latest_underived: LatestUnderivedBookmarkEntry,
    pub latest_log_id: Option<BookmarkUpdateLogId>,
    pub derived_entries: EnumMap<WarmerRequirement, LatestDerivedBookmarkEntry>,
}

/// Searches bookmark log for latest entry for which everything is derived. Note that we consider log entry that
/// deletes a bookmark to be derived.
pub async fn find_latest_derived_and_underived(
    ctx: &CoreContext,
    bookmarks: &dyn Bookmarks,
    bookmark_update_log: &dyn BookmarkUpdateLog,
    book: &BookmarkKey,
    warmers: &[Warmer],
) -> Result<LatestBookmarkEntries, Error> {
    find_latest_derived_and_underived_with_freshness(
        ctx,
        bookmarks,
        bookmark_update_log,
        book,
        warmers,
        Freshness::MaybeStale,
    )
    .await
}

async fn find_latest_derived_and_underived_with_freshness(
    ctx: &CoreContext,
    bookmarks: &dyn Bookmarks,
    bookmark_update_log: &dyn BookmarkUpdateLog,
    book: &BookmarkKey,
    warmers: &[Warmer],
    freshness: Freshness,
) -> Result<LatestBookmarkEntries, Error> {
    let mut latest_underived = LatestUnderivedBookmarkEntry::default();
    let mut latest_log_id = None;
    let mut derived_entries = EnumMap::<WarmerTag, Option<LatestDerivedBookmarkEntry>>::default();
    let mut found_latest_entry = false;
    let history_depth_limits = vec![0, 10, 50, 100, 1000, 10000];

    for (prev_limit, limit) in history_depth_limits.into_iter().tuple_windows() {
        if prev_limit > 0 {
            tracing::debug!("{} bookmark, limit {}", book, limit);
        }
        // Note that since new entries might be inserted to the bookmark log,
        // the next call to `list_bookmark_log_entries(...)` might return
        // entries that were already returned on the previous call `list_bookmark_log_entries(...)`.
        // That means that the same entry might be rechecked, but that shouldn't be a big problem.
        let mut log_entries = bookmark_update_log
            .list_bookmark_log_entries(
                ctx.clone(),
                book.clone(),
                limit,
                Some(prev_limit),
                freshness,
            )
            .map_ok(|(id, maybe_cs_id, _, ts)| {
                let id = id.into();
                (maybe_cs_id, Some((id, ts)))
            })
            .try_collect::<Vec<_>>()
            .await?;

        if log_entries.is_empty() {
            tracing::debug!("bookmark {} has no history in the log", book);
            let maybe_cs_id = bookmarks
                .get(ctx.clone(), book, bookmarks::Freshness::MostRecent)
                .await?;
            // If a bookmark has no history then we add a synthetic entry with no timestamp or
            // log ID. Every real log entry above carries `Some((id, timestamp))`.
            log_entries.push((maybe_cs_id, None));
        }

        let log_entries_fetched = log_entries.len();
        if !found_latest_entry {
            if let Some((maybe_cs_id, id_and_ts)) = log_entries.first() {
                latest_underived.maybe_cs_id = *maybe_cs_id;
                latest_log_id = id_and_ts.as_ref().map(|(id, _)| *id);
                found_latest_entry = true;
            }
        }

        let mut maybe_derived = stream::iter(log_entries.into_iter().map(
            |(maybe_cs_id, id_and_ts)| async move {
                match maybe_cs_id {
                    Some(cs_id) => {
                        let derived = get_warm_state(ctx, cs_id, warmers).await;
                        (Some((cs_id, id_and_ts)), derived)
                    }
                    None => (None, Ok(WarmState::empty())),
                }
            },
        ))
        .buffered(100);

        while let Some((maybe_cs_id_ts, warm_state)) = maybe_derived.next().watched().await {
            let warm_state = warm_state?;
            let maybe_cs_ts =
                maybe_cs_id_ts.map(|(cs_id, id_and_ts)| (cs_id, id_and_ts.map(|(_, ts)| ts)));

            for (tag, maybe_entry) in derived_entries {
                if maybe_entry.is_none() && warm_state.is_warm(tag) {
                    derived_entries[tag] = Some(LatestDerivedBookmarkEntry::Found(maybe_cs_ts));
                }
            }

            if warm_state.are_all_warm() {
                let latest_all = LatestDerivedBookmarkEntry::Found(maybe_cs_ts);
                return Ok(LatestBookmarkEntries {
                    latest_underived,
                    latest_log_id,
                    derived_entries: enum_map::enum_map! {
                        // If they are all warm, the unwrap sections should never run
                        WarmerRequirement::HgOnly => derived_entries[WarmerTag::Hg].unwrap_or(LatestDerivedBookmarkEntry::Found(None)),
                        WarmerRequirement::GitOnly => derived_entries[WarmerTag::Git].unwrap_or(LatestDerivedBookmarkEntry::Found(None)),
                        WarmerRequirement::AllKinds => latest_all,
                    },
                });
            } else {
                // Store the oldest underived bookmarke entry ID and ts for logging purpose
                latest_underived.maybe_id_ts = maybe_cs_id_ts.and_then(|(_, id_and_ts)| id_and_ts);
            }
        }

        // Bookmark has been created recently - return what we found for each scope
        if (log_entries_fetched as u32) < limit {
            return Ok(LatestBookmarkEntries {
                latest_underived,
                latest_log_id,
                derived_entries: enum_map::enum_map! {
                    WarmerRequirement::HgOnly => derived_entries[WarmerTag::Hg].unwrap_or(LatestDerivedBookmarkEntry::Found(None)),
                    WarmerRequirement::GitOnly => derived_entries[WarmerTag::Git].unwrap_or(LatestDerivedBookmarkEntry::Found(None)),
                    WarmerRequirement::AllKinds => LatestDerivedBookmarkEntry::Found(None),
                },
            });
        }
    }

    Ok(LatestBookmarkEntries {
        latest_underived,
        latest_log_id,
        derived_entries: enum_map::enum_map! {
            WarmerRequirement::HgOnly => derived_entries[WarmerTag::Hg].unwrap_or(LatestDerivedBookmarkEntry::NotFound),
            WarmerRequirement::GitOnly => derived_entries[WarmerTag::Git].unwrap_or(LatestDerivedBookmarkEntry::NotFound),
            WarmerRequirement::AllKinds => LatestDerivedBookmarkEntry::NotFound,
        },
    })
}

#[facet::container]
#[derive(Clone)]
struct BookmarksCoordinatorRepo {
    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    bookmark_update_log: dyn BookmarkUpdateLog,

    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    repo_event_publisher: dyn RepoEventPublisher,
}

struct BookmarksCoordinator {
    bookmarks: Arc<RwLock<HashMap<BookmarkKey, BookmarkState>>>,
    repo: BookmarksCoordinatorRepo,
    warmers: Arc<Vec<Warmer>>,
}

impl BookmarksCoordinator {
    fn new(
        bookmarks: Arc<RwLock<HashMap<BookmarkKey, BookmarkState>>>,
        bookmarks_fetcher: ArcBookmarks,
        bookmark_update_log: ArcBookmarkUpdateLog,
        repo_identity: ArcRepoIdentity,
        repo_event_publisher: ArcRepoEventPublisher,
        warmers: Arc<Vec<Warmer>>,
    ) -> Self {
        let repo = BookmarksCoordinatorRepo {
            bookmarks: bookmarks_fetcher,
            bookmark_update_log,
            repo_identity,
            repo_event_publisher,
        };

        Self {
            bookmarks,
            repo,
            warmers,
        }
    }

    #[cfg(fbcode_build)]
    fn spawn_event_router(
        self,
        ctx: CoreContext,
        terminate: oneshot::Receiver<()>,
        sync: SyncSignals,
        subscriber: Option<broadcast::Receiver<BookmarkEventNotification>>,
    ) {
        let repo_name = self.repo.repo_identity().name().to_owned();
        let settings = BatchSettings::from_justknobs(&repo_name);
        let span = tracing::info_span!("wbc", repo = %repo_name, source = "event_router");
        let fut = async move {
            tracing::info!("Started EventRouter warm bookmark cache updater");
            let _activity = event_router_coordinator_stats::EventRouterCoordinatorActivity::new(
                ctx.fb,
                repo_name.clone(),
            );
            let batches = self
                .run_event_router_batches(&ctx, &sync, settings, subscriber)
                .boxed();
            let _ = select(batches, terminate).await;
            STATS::max_staleness_secs.set_value(ctx.fb, 0, (repo_name.clone(),));
            tracing::info!("Stopped EventRouter warm bookmark cache updater");
        }
        .instrument(span);

        std::mem::drop(mononoke::spawn_task(fut));
    }
}

#[cfg(fbcode_build)]
async fn warm_bookmark_from_notification(
    ctx: &CoreContext,
    key: &BookmarkKey,
    changeset_id: ChangesetId,
    log_id: BookmarkUpdateLogId,
    warmers: &[Warmer],
) -> Result<(), Error> {
    let ctx = ctx.clone().with_mutated_scuba(|mut scuba| {
        scuba.add("bookmark", key.to_string());
        scuba.add("bookmark_log_id", u64::from(log_id));
        scuba.add("top_changeset", changeset_id.to_hex().to_string());
        scuba
    });
    let (stats, result) = warm_all(&ctx, changeset_id, warmers).timed().await;
    ctx.scuba()
        .clone()
        .add_future_stats(&stats)
        .log_with_msg("After warming bookmark from notification", None);
    result.with_context(|| {
        format!("deriving data for {changeset_id} while updating {key} from notification")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::BookmarkUpdateLog;
    use bookmarks::BookmarkUpdateLogArc;
    use bookmarks::Bookmarks;
    use bookmarks::BookmarksArc;
    use bookmarks::BookmarksMaybeStaleExt;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use delayblob::DelayedBlobstore;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use fixtures::Linear;
    use fixtures::TestRepoFixture;
    use maplit::hashmap;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use phases::Phases;
    use phases::PhasesArc;
    use phases::PhasesRef;
    use repo_blobstore::RepoBlobstore;
    use repo_derivation_queues::DerivationPriority;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataArc;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_event_publisher::RepoEventPublisher;
    use repo_event_publisher::RepoEventPublisherArc;
    use repo_identity::RepoIdentity;
    use repo_identity::RepoIdentityArc;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::bookmark;
    use tests_utils::resolve_cs_id;

    use super::*;

    #[facet::container]
    #[derive(Clone)]
    pub struct Repo(
        RepoIdentity,
        RepoBlobstore,
        RepoDerivedData,
        CommitGraph,
        dyn CommitGraphWriter,
        dyn BonsaiHgMapping,
        dyn BookmarkUpdateLog,
        dyn Bookmarks,
        dyn RepoEventPublisher,
        FilestoreConfig,
        dyn Phases,
    );

    #[mononoke::fbinit_test]
    async fn test_public_phase_readiness_does_not_derive(fb: FacebookInit) -> Result<(), Error> {
        let repo: Repo = TestRepoFactory::new(fb)?.build().await?;
        let ctx = CoreContext::test_mock(fb);
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .set_message("parent")
            .commit()
            .await?;
        let head = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .set_message("head")
            .commit()
            .await?;
        let other = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .set_message("other")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "master").set_to(head).await?;
        bookmark(&ctx, &repo, "other").set_to(other).await?;
        let warmers = vec![create_public_phase_warmer(&ctx, repo.phases_arc())];

        assert!(
            warm_changesets(&ctx, [parent, head, other], &warmers)
                .await
                .is_empty(),
            "readiness checks must not derive missing public phases"
        );
        assert!(repo.phases().list_all_public(&ctx).await?.is_empty());

        warm_all(&ctx, head, &warmers).await?;

        assert_eq!(
            warm_changesets(&ctx, [parent, head, other], &warmers).await,
            HashSet::from([parent, head]),
            "explicit warming must mark only the requested ancestry"
        );
        assert_eq!(
            repo.phases()
                .list_all_public(&ctx)
                .await?
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([parent, head])
        );
        Ok(())
    }

    #[cfg(fbcode_build)]
    async fn apply_notification(
        coordinator: &BookmarksCoordinator,
        ctx: &CoreContext,
        notification: BookmarkEventNotification,
    ) -> Result<(), Error> {
        let parsed = coordinator.parse_notification(notification)?;
        coordinator
            .notification_batch(
                ctx.clone(),
                HashMap::from([(parsed.key.clone(), parsed)]),
                std::num::NonZeroUsize::MIN,
            )
            .await;
        Ok(())
    }

    #[cfg(fbcode_build)]
    async fn bookmark_log_id(
        ctx: &CoreContext,
        repo: &Repo,
        bookmark_name: &str,
    ) -> Result<BookmarkUpdateLogId, Error> {
        let key = BookmarkKey::new(bookmark_name)?;
        let (log_id, _, _, _) = repo
            .bookmark_update_log()
            .list_bookmark_log_entries(ctx.clone(), key, 1, None, Freshness::MostRecent)
            .try_next()
            .await?
            .with_context(|| format!("bookmark log entry for {bookmark_name} was not found"))?;
        Ok(BookmarkUpdateLogId(log_id))
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    async fn test_update_from_notification_handles_create_move_and_delete(
        fb: FacebookInit,
    ) -> Result<(), Error> {
        let repo: Repo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);
        let bookmarks = repo
            .bookmarks()
            .get_publishing_bookmarks_maybe_stale(ctx.clone())
            .map_ok(|(book, cs_id)| {
                let kind = *book.kind();
                (
                    book.into_key(),
                    test_bookmark_state_all_warmers(kind, cs_id),
                )
            })
            .try_collect::<HashMap<_, _>>()
            .await?;
        let bookmarks = Arc::new(RwLock::new(bookmarks));
        let warmers = Arc::new(vec![create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        )]);
        let coordinator = BookmarksCoordinator::new(
            bookmarks.clone(),
            repo.bookmarks_arc(),
            repo.bookmark_update_log_arc(),
            repo.repo_identity_arc(),
            repo.repo_event_publisher_arc(),
            warmers,
        );

        let first = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("first-notification", "first")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "master").set_to(first).await?;

        let latest = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("latest-notification", "latest")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "master").set_to(latest).await?;
        let move_log_id = bookmark_log_id(&ctx, &repo, "master").await?;

        let notification = BookmarkEventNotification {
            repo_id: repo.repo_identity().id().id(),
            bookmark_name: "master".to_owned(),
            bookmark_category: EventRouterBookmarkCategory::BRANCH,
            log_id: i64::try_from(move_log_id)?,
            from_changeset_id: Some(first.as_ref().to_vec()),
            to_changeset_id: Some(latest.as_ref().to_vec()),
            ..Default::default()
        };
        apply_notification(&coordinator, &ctx, notification).await?;

        let master_key = BookmarkKey::new("master")?;
        assert_eq!(
            bookmarks.with_read(|bookmarks| bookmarks.get(&master_key).cloned()),
            Some(
                test_bookmark_state_all_warmers_with_latest_log_id(
                    &ctx,
                    &repo,
                    "master",
                    BookmarkKind::PullDefaultPublishing,
                    latest,
                )
                .await?,
            )
        );

        let created = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("created-notification", "created")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "created")
            .create_publishing(created)
            .await?;
        let create_log_id = bookmark_log_id(&ctx, &repo, "created").await?;
        apply_notification(
            &coordinator,
            &ctx,
            BookmarkEventNotification {
                repo_id: repo.repo_identity().id().id(),
                bookmark_name: "created".to_owned(),
                bookmark_category: EventRouterBookmarkCategory::BRANCH,
                log_id: i64::try_from(create_log_id)?,
                from_changeset_id: None,
                to_changeset_id: Some(created.as_ref().to_vec()),
                ..Default::default()
            },
        )
        .await?;
        let created_key = BookmarkKey::new("created")?;
        assert_eq!(
            bookmarks.with_read(|bookmarks| bookmarks.get(&created_key).cloned()),
            Some(
                test_bookmark_state_all_warmers_with_latest_log_id(
                    &ctx,
                    &repo,
                    "created",
                    BookmarkKind::Publishing,
                    created,
                )
                .await?,
            )
        );

        bookmark(&ctx, &repo, "master").delete().await?;
        let delete_log_id = bookmark_log_id(&ctx, &repo, "master").await?;
        apply_notification(
            &coordinator,
            &ctx,
            BookmarkEventNotification {
                repo_id: repo.repo_identity().id().id(),
                bookmark_name: "master".to_owned(),
                bookmark_category: EventRouterBookmarkCategory::BRANCH,
                log_id: i64::try_from(delete_log_id)?,
                from_changeset_id: Some(latest.as_ref().to_vec()),
                to_changeset_id: None,
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            bookmarks.with_read(|bookmarks| bookmarks.get(&master_key).cloned()),
            None,
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_simple(fb: FacebookInit) -> Result<(), Error> {
        let repo: Repo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);

        let sub = repo
            .bookmarks()
            .create_subscription(&ctx, Freshness::MostRecent)
            .await?;

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        // Unodes haven't been derived at all - so we should get an empty set of bookmarks
        let bookmarks = init_bookmarks(
            &ctx,
            &*sub,
            repo.bookmarks(),
            repo.bookmark_update_log(),
            &warmers,
            InitMode::Rewind,
        )
        .await?;
        assert_eq!(bookmarks, HashMap::new());

        let master_cs_id = resolve_cs_id(&ctx, &repo, "master").await?;
        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(&ctx, master_cs_id, DerivationPriority::LOW)
            .await?;

        let bookmarks = init_bookmarks(
            &ctx,
            &*sub,
            repo.bookmarks(),
            repo.bookmark_update_log(),
            &warmers,
            InitMode::Rewind,
        )
        .await?;
        assert_eq!(
            bookmarks,
            hashmap! {BookmarkKey::new("master")? => test_bookmark_state_all_warmers_with_latest_log_id(
                &ctx,
                &repo,
                "master",
                BookmarkKind::PullDefaultPublishing,
                master_cs_id,
            ).await?}
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_derived(fb: FacebookInit) -> Result<(), Error> {
        let put_distr = rand_distr::Normal::<f64>::new(0.1, 0.05).unwrap();
        let get_distr = rand_distr::Normal::<f64>::new(0.05, 0.025).unwrap();
        let blobstore = Arc::new(DelayedBlobstore::new(
            Memblob::default(),
            put_distr,
            get_distr,
        ));
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(blobstore)
            .build()
            .await?;
        Linear::init_repo(fb, &repo).await?;
        let ctx = CoreContext::test_mock(fb);

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        tracing::info!("creating 5 derived commits");
        let mut master = resolve_cs_id(&ctx, &repo, "master").await?;
        for _ in 1..5 {
            let new_master = CreateCommitContext::new(&ctx, &repo, vec![master])
                .commit()
                .await?;

            bookmark(&ctx, &repo, "master").set_to(new_master).await?;
            master = new_master;
        }
        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(&ctx, master, DerivationPriority::LOW)
            .await?;
        let derived_master = master;

        tracing::info!("creating 5 more underived commits");
        for _ in 1..5 {
            let new_master = CreateCommitContext::new(&ctx, &repo, vec![master])
                .commit()
                .await?;
            bookmark(&ctx, &repo, "master").set_to(new_master).await?;
            master = new_master;
        }

        let sub = repo
            .bookmarks()
            .create_subscription(&ctx, Freshness::MostRecent)
            .await?;

        let bookmarks = init_bookmarks(
            &ctx,
            &*sub,
            repo.bookmarks(),
            repo.bookmark_update_log(),
            &warmers,
            InitMode::Rewind,
        )
        .await?;
        assert_eq!(
            bookmarks,
            hashmap! {BookmarkKey::new("master")? => test_bookmark_state_all_warmers(
                BookmarkKind::PullDefaultPublishing,
                derived_master,
            )}
        );

        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(&ctx, master, DerivationPriority::LOW)
            .await?;
        let bookmarks = init_bookmarks(
            &ctx,
            &*sub,
            repo.bookmarks(),
            repo.bookmark_update_log(),
            &warmers,
            InitMode::Rewind,
        )
        .await?;
        assert_eq!(
            bookmarks,
            hashmap! {BookmarkKey::new("master")? => test_bookmark_state_all_warmers_with_latest_log_id(
                &ctx,
                &repo,
                "master",
                BookmarkKind::PullDefaultPublishing,
                master,
            ).await?}
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_a_lot_of_moves(fb: FacebookInit) -> Result<(), Error> {
        let repo: Repo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        let derived_master = resolve_cs_id(&ctx, &repo, "master").await?;
        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(&ctx, derived_master, DerivationPriority::LOW)
            .await?;

        for i in 1..50 {
            let new_master = CreateCommitContext::new(&ctx, &repo, vec!["master"])
                .add_file(format!("{i}").as_str(), "content")
                .commit()
                .await?;

            bookmark(&ctx, &repo, "master").set_to(new_master).await?;
        }

        let sub = repo
            .bookmarks()
            .create_subscription(&ctx, Freshness::MostRecent)
            .await?;

        let bookmarks = init_bookmarks(
            &ctx,
            &*sub,
            repo.bookmarks(),
            repo.bookmark_update_log(),
            &warmers,
            InitMode::Rewind,
        )
        .await?;
        assert_eq!(
            bookmarks,
            hashmap! {BookmarkKey::new("master")? => test_bookmark_state_all_warmers(
                BookmarkKind::PullDefaultPublishing,
                derived_master,
            )}
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_derived_right_after_threshold(fb: FacebookInit) -> Result<(), Error> {
        let repo: Repo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        let derived_master = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("somefile", "content")
            .commit()
            .await?;
        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(&ctx, derived_master, DerivationPriority::LOW)
            .await?;
        bookmark(&ctx, &repo, "master")
            .set_to(derived_master)
            .await?;

        // First history threshold is 10. Let's make sure we don't have off-by one errors
        for i in 0..10 {
            let new_master = CreateCommitContext::new(&ctx, &repo, vec!["master"])
                .add_file(format!("{i}").as_str(), "content")
                .commit()
                .await?;

            bookmark(&ctx, &repo, "master").set_to(new_master).await?;
        }

        let sub = repo
            .bookmarks()
            .create_subscription(&ctx, Freshness::MostRecent)
            .await?;

        let bookmarks = init_bookmarks(
            &ctx,
            &*sub,
            repo.bookmarks(),
            repo.bookmark_update_log(),
            &warmers,
            InitMode::Rewind,
        )
        .await?;
        assert_eq!(
            bookmarks,
            hashmap! {BookmarkKey::new("master")? => test_bookmark_state_all_warmers(
                BookmarkKind::PullDefaultPublishing,
                derived_master,
            )}
        );

        Ok(())
    }

    async fn test_bookmark_state_all_warmers_with_latest_log_id(
        ctx: &CoreContext,
        repo: &Repo,
        bookmark: &str,
        kind: BookmarkKind,
        cs_id: ChangesetId,
    ) -> Result<BookmarkState, Error> {
        let mut state = test_bookmark_state_all_warmers(kind, cs_id);
        let subscription = repo
            .bookmarks()
            .create_subscription(ctx, Freshness::MostRecent)
            .await?;
        let key = BookmarkKey::new(bookmark)?;
        if let Some((current_cs_id, _, Some(log_id))) = subscription.bookmarks().get(&key)
            && *current_cs_id == cs_id
        {
            state.set_log_id(*log_id);
        }
        Ok(state)
    }

    // Test helper to create a BookmarkState with all warmers enabled
    fn test_bookmark_state_all_warmers(kind: BookmarkKind, cs_id: ChangesetId) -> BookmarkState {
        let warmers = vec![Warmer {
            warmer: Box::new(|_ctx, _cs_id| Box::pin(async { Ok(()) })),
            is_warm: Box::new(|_ctx, _cs_id| Box::pin(async { Ok(true) })),
            name: "test".to_string(),
            tags: vec![WarmerTag::Hg, WarmerTag::Git],
        }];
        let mut state = BookmarkState::from_warmers(kind, &warmers);
        state.set_all_tracked(cs_id);
        state
    }

    /// Wraps `inner` so that every `is_warm` call is counted.
    fn counting_warmer(inner: Warmer, count: Arc<AtomicUsize>) -> Warmer {
        let Warmer {
            warmer,
            is_warm,
            name,
            tags,
        } = inner;
        Warmer {
            warmer,
            is_warm: Box::new(move |ctx: &CoreContext, cs_id: ChangesetId| {
                count.fetch_add(1, Ordering::Relaxed);
                is_warm(ctx, cs_id)
            }),
            name,
            tags,
        }
    }

    /// Adds `commits` derived commits on top of master, points
    /// `bookmarks_per_commit` bookmarks at each of them, and adds an underived
    /// `feature` bookmark. Returns the bookmarked derived commits.
    async fn add_duplicate_bookmarks(
        ctx: &CoreContext,
        repo: &Repo,
        commits: usize,
        bookmarks_per_commit: usize,
    ) -> Result<Vec<ChangesetId>, Error> {
        let mut parent = resolve_cs_id(ctx, repo, "master").await?;
        let mut trunk = Vec::with_capacity(commits);
        for i in 0..commits {
            parent = CreateCommitContext::new(ctx, repo, vec![parent])
                .add_file(format!("file{i}").as_str(), format!("content{i}"))
                .commit()
                .await?;
            for j in 0..bookmarks_per_commit {
                bookmark(ctx, repo, format!("tags/v{i}.{j}").as_str())
                    .set_to(parent)
                    .await?;
            }
            trunk.push(parent);
        }
        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(ctx, parent, DerivationPriority::LOW)
            .await?;

        let feature = CreateCommitContext::new(ctx, repo, vec![parent])
            .add_file("feature", "feature")
            .commit()
            .await?;
        bookmark(ctx, repo, "feature").set_to(feature).await?;
        Ok(trunk)
    }

    /// Initializes the cache with a counting unodes warmer. Returns the cache
    /// contents, the number of warm checks and the elapsed time, after
    /// asserting that every bookmark on a derived commit is warm at its own
    /// commit and log entry.
    async fn init_and_count(
        ctx: &CoreContext,
        repo: &Repo,
    ) -> Result<(HashMap<BookmarkKey, BookmarkState>, usize, Duration), Error> {
        let count = Arc::new(AtomicUsize::new(0));
        let warmers = Arc::new(vec![counting_warmer(
            create_derived_data_warmer::<RootUnodeManifestId>(
                ctx,
                repo.repo_derived_data_arc(),
                vec![WarmerTag::Hg, WarmerTag::Git],
            ),
            count.clone(),
        )]);
        let sub = repo
            .bookmarks()
            .create_subscription(ctx, Freshness::MostRecent)
            .await?;

        let start = std::time::Instant::now();
        let cache = init_bookmarks(
            ctx,
            &*sub,
            repo.bookmarks(),
            repo.bookmark_update_log(),
            &warmers,
            InitMode::Rewind,
        )
        .await?;
        let elapsed = start.elapsed();

        for (key, (cs_id, kind, log_id)) in sub.bookmarks() {
            if key.name().as_str() == "feature" {
                continue;
            }
            let mut expected = test_bookmark_state_all_warmers(*kind, *cs_id);
            if let Some(log_id) = log_id {
                expected.set_log_id(*log_id);
            }
            assert_eq!(cache.get(key), Some(&expected), "bookmark {key}");
        }
        Ok((cache, count.load(Ordering::Relaxed), elapsed))
    }

    #[mononoke::fbinit_test]
    async fn test_init_checks_each_changeset_once(fb: FacebookInit) -> Result<(), Error> {
        let repo: Repo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);
        add_duplicate_bookmarks(&ctx, &repo, 4, 50).await?;

        let (cache, checks, _) = init_and_count(&ctx, &repo).await?;

        // 200 tags plus master are warm; feature is underived and absent.
        assert_eq!(cache.len(), 201);
        assert!(!cache.contains_key(&BookmarkKey::new("feature")?));
        // The 6 distinct bookmarked commits (4 tagged, master, feature) are
        // checked once each, plus a few checks while walking feature back.
        assert!(
            checks <= 9,
            "expected one warm check per distinct commit, got {checks}"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_init_with_duplicate_bookmarks_slow_blobstore(
        fb: FacebookInit,
    ) -> Result<(), Error> {
        // 10ms per blobstore read, roughly a cachelib miss served by memcache.
        let get_distr = rand_distr::Normal::<f64>::new(0.01, 0.002).unwrap();
        let put_distr = rand_distr::Normal::<f64>::new(0.0, 0.000001).unwrap();
        let blobstore = Arc::new(DelayedBlobstore::new(
            Memblob::default(),
            get_distr,
            put_distr,
        ));
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(blobstore)
            .build()
            .await?;
        Linear::init_repo(fb, &repo).await?;
        let ctx = CoreContext::test_mock(fb);
        // 95% of bookmarks repeat a commit, the median for repos with
        // 5k-20k bookmarks.
        add_duplicate_bookmarks(&ctx, &repo, 100, 20).await?;

        let (cache, checks, elapsed) = init_and_count(&ctx, &repo).await?;
        println!(
            "{} bookmarks on 101 commits, 10ms reads: {checks} warm checks in {elapsed:?}",
            cache.len() + 1
        );
        Ok(())
    }
}
