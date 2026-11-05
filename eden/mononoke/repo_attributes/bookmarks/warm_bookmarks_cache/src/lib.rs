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
#[path = "facebook/event_router_coordinator_stats.rs"]
mod event_router_coordinator_stats;
#[cfg(test)]
mod scoped_bookmarks_tests;
#[cfg(test)]
mod test_helpers;

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::RangeBounds;
use std::sync::Arc;
#[cfg(fbcode_build)]
use std::sync::LazyLock;
use std::sync::RwLock;
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
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Bookmarks;
use bookmarks::BookmarksRef;
use bookmarks::BookmarksSubscription;
use bookmarks::Freshness;
use bookmarks_cache::BookmarksCache;
use bookmarks_cache::ScopedBookmarksCache;
use bookmarks_cache::WarmerRequirement;
use bookmarks_types::Bookmark;
use bookmarks_types::BookmarkKind;
#[cfg(fbcode_build)]
use bookmarks_types::BookmarkName;
use bookmarks_types::BookmarkPagination;
use bookmarks_types::BookmarkPrefix;
use case_conflict_skeleton_manifest::RootCaseConflictSkeletonManifestId;
use changeset_info::ChangesetInfo;
use cloned::cloned;
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
use futures::future;
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
#[cfg(fbcode_build)]
use repo_event_publisher::BookmarkCategory as EventRouterBookmarkCategory;
#[cfg(fbcode_build)]
use repo_event_publisher::BookmarkEventNotification;
use repo_event_publisher::RepoEventPublisher;
use repo_identity::ArcRepoIdentity;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use skeleton_manifest::RootSkeletonManifestId;
use skeleton_manifest_v2::RootSkeletonManifestV2Id;
use stats::prelude::*;
use tokio::sync::Notify;
#[cfg(fbcode_build)]
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::Instrument;
use unodes::RootUnodeManifestId;

mod warmers;
pub use warmers::create_derived_data_warmer;
pub use warmers::create_public_phase_warmer;

#[cfg(fbcode_build)]
static WBC_INSTRUMENT: LazyLock<Instrument_MononokeWarmBookmarkCacheStats> =
    LazyLock::new(Instrument_MononokeWarmBookmarkCacheStats::new);

#[cfg(fbcode_build)]
struct ParsedBookmarkEventNotification {
    key: BookmarkKey,
    log_id: BookmarkUpdateLogId,
    from_changeset_id: Option<ChangesetId>,
    to_changeset_id: Option<ChangesetId>,
}

#[cfg(fbcode_build)]
impl TryFrom<BookmarkEventNotification> for ParsedBookmarkEventNotification {
    type Error = Error;

    fn try_from(notification: BookmarkEventNotification) -> Result<Self, Self::Error> {
        let category = if notification.bookmark_category == EventRouterBookmarkCategory::BRANCH {
            BookmarkCategory::Branch
        } else if notification.bookmark_category == EventRouterBookmarkCategory::TAG {
            BookmarkCategory::Tag
        } else if notification.bookmark_category == EventRouterBookmarkCategory::NOTE {
            BookmarkCategory::Note
        } else {
            return Err(anyhow!(
                "unknown EventRouter bookmark category {:?}",
                notification.bookmark_category,
            ));
        };
        let bookmark_name = BookmarkName::new(notification.bookmark_name)?;
        let key = BookmarkKey::with_name_and_category(bookmark_name, category);
        let log_id = BookmarkUpdateLogId::try_from(notification.log_id)
            .context("bookmark notification has invalid log ID")?;
        let from_changeset_id = notification
            .from_changeset_id
            .as_deref()
            .map(ChangesetId::from_bytes)
            .transpose()
            .context("bookmark notification has invalid from-changeset ID")?;
        let to_changeset_id = notification
            .to_changeset_id
            .as_deref()
            .map(ChangesetId::from_bytes)
            .transpose()
            .context("bookmark notification has invalid to-changeset ID")?;

        Ok(Self {
            key,
            log_id,
            from_changeset_id,
            to_changeset_id,
        })
    }
}

#[cfg(fbcode_build)]
enum EventRouterCoordinatorUpdate {
    FullRefresh(&'static str),
    Notification(BookmarkEventNotification),
}

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
    event_router_coordinator_enabled: bool,
    event_router_batching_enabled: bool,
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
            event_router_coordinator_enabled: false,
            event_router_batching_enabled: false,
            warmers: vec![],
            init_mode: InitMode::Rewind,
            default_warmer_requirement: warmer_requirement,
        }
    }

    /// Selects EventRouter notifications as the WBC coordinator input.
    pub fn with_event_router_coordinator_enabled(mut self, enabled: bool) -> Self {
        self.event_router_coordinator_enabled = enabled;
        self
    }

    /// Enables batched EventRouter processing with JK-controlled concurrency and retention.
    /// Requires the EventRouter coordinator to be enabled.
    pub fn with_event_router_batching_enabled(mut self, enabled: bool) -> Self {
        self.event_router_batching_enabled = enabled;
        self
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
        event_router_coordinator_enabled: bool,
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
            event_router_coordinator_enabled,
            event_router_batching_enabled: false,
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
            event_router_coordinator_enabled,
            event_router_batching_enabled,
            warmers,
            init_mode,
            default_warmer_requirement,
        } = builder;
        if event_router_batching_enabled && !event_router_coordinator_enabled {
            return Err(anyhow!(
                "WBC batching requires EventRouter notification processing"
            ));
        }
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
            sub,
            bookmarks.clone(),
            bookmark_update_log.clone(),
            repo_identity.clone(),
            repo_event_publisher.clone(),
            warmers.clone(),
        );
        #[cfg(not(fbcode_build))]
        let _ = event_router_coordinator_enabled;
        #[cfg(fbcode_build)]
        if event_router_coordinator_enabled {
            coordinator.spawn_event_router(
                ctx.clone(),
                receiver,
                (notify_sync_start.clone(), notify_sync_complete.clone()),
                event_router_batching_enabled,
            );
        } else {
            coordinator.spawn(
                ctx.clone(),
                receiver,
                notify_sync_start.clone(),
                notify_sync_complete.clone(),
            );
        }
        #[cfg(not(fbcode_build))]
        coordinator.spawn(
            ctx.clone(),
            receiver,
            notify_sync_start.clone(),
            notify_sync_complete.clone(),
        );
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
    sub: Box<dyn BookmarksSubscription>,
    repo: BookmarksCoordinatorRepo,
    warmers: Arc<Vec<Warmer>>,
    live_updaters: Arc<RwLock<HashMap<BookmarkKey, BookmarkUpdaterState>>>,
    updaters_handles: HashMap<BookmarkKey, JoinHandle<()>>,
}

impl BookmarksCoordinator {
    fn new(
        bookmarks: Arc<RwLock<HashMap<BookmarkKey, BookmarkState>>>,
        sub: Box<dyn BookmarksSubscription>,
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
            sub,
            repo,
            warmers,
            live_updaters: Arc::new(RwLock::new(HashMap::new())),
            updaters_handles: Default::default(),
        }
    }

    async fn update(&mut self, ctx: &CoreContext) -> Result<(), Error> {
        report_delay_and_remove_finished_updaters(
            ctx,
            &self.live_updaters,
            self.repo.repo_identity().name(),
        );

        let cur_bookmarks = self.bookmarks.with_read(|bookmarks| bookmarks.clone());

        self.sub
            .refresh(ctx)
            .await
            .context("Error refreshing subscription")?;

        let new_bookmarks = Cow::Borrowed(self.sub.bookmarks());

        let mut changed_bookmarks = vec![];
        // Find bookmarks that were moved/created and spawn an updater
        // for them
        for (key, new_value) in new_bookmarks.iter() {
            let cur_value = cur_bookmarks.get(key);
            let changed = match cur_value {
                Some(state) => {
                    let current_cs_id = state.get(WarmerRequirement::AllKinds)?;
                    current_cs_id != Some(new_value.0) || state.kind != new_value.1
                }
                None => true,
            };
            if changed {
                let book = Bookmark::new(key.clone(), new_value.1);
                changed_bookmarks.push(book);
            }
        }

        // Find bookmarks that were deleted
        for (key, cur_value) in &cur_bookmarks {
            // There's a potential race condition if a bookmark was deleted
            // and then immediately recreated with another kind (e.g. Publishing instead of
            // PullDefault). Because of the race WarmBookmarksCache might store a
            // bookmark with an old Kind.
            // I think this is acceptable because it's going to be fixed on next iteration
            // of the loop, and because fixing it properly is hard if not impossible -
            // change of bookmark kind is not reflected in update log.
            if !new_bookmarks.contains_key(key) {
                let book = Bookmark::new(key.clone(), cur_value.kind);
                changed_bookmarks.push(book);
            }
        }

        for book in changed_bookmarks {
            let need_spawning = self.live_updaters.with_write(|live_updaters| {
                if !live_updaters.contains_key(book.key()) {
                    live_updaters.insert(book.key().clone(), BookmarkUpdaterState::Started);
                    true
                } else {
                    false
                }
            });
            // It's possible that bookmark updater removes a bookmark right after
            // we tried to insert it. That means we won't spawn a new updater,
            // but that's not a big deal though - we'll do it on the next iteration
            // of the loop
            if need_spawning {
                cloned!(
                    ctx,
                    self.repo,
                    self.bookmarks,
                    self.live_updaters,
                    self.warmers,
                );
                self.updaters_handles.insert(
                    book.key().clone(),
                    mononoke::spawn_task(async move {
                        let res = single_bookmark_updater(
                            &ctx,
                            &repo,
                            &book,
                            &bookmarks,
                            &warmers,
                            |ts: Timestamp| {
                                live_updaters.with_write(|live_updaters| {
                                    live_updaters.insert(
                                        book.key().clone(),
                                        BookmarkUpdaterState::InProgress {
                                            oldest_underived_ts: ts,
                                        },
                                    );
                                });
                            },
                        )
                        .await;
                        if let Err(ref err) = res {
                            #[cfg(fbcode_build)]
                            WBC_INSTRUMENT.observe(MononokeWarmBookmarkCacheStats {
                                event: Some(WarmBookmarkCacheEvent::UpdateFailure),
                                ..Default::default()
                            });
                            tracing::warn!("update of {} failed: {:?}", book.key(), err);
                        };

                        live_updaters.with_write(|live_updaters| {
                            let maybe_state = live_updaters.remove(book.key());
                            if let Some(state) = maybe_state {
                                live_updaters.insert(book.key().clone(), state.into_finished(&res));
                            }
                        });
                    }),
                );
            }
        }

        Ok(())
    }

    #[cfg(fbcode_build)]
    async fn update_inline_from_notification(
        &mut self,
        ctx: &CoreContext,
        notification: BookmarkEventNotification,
    ) -> Result<(), Error> {
        let notification = self.parse_notification(notification)?;
        let key = notification.key.clone();
        let log_id = notification.log_id;

        if let Some(handle) = self.updaters_handles.remove(&key) {
            handle
                .await
                .with_context(|| format!("joining existing updater for {key}"))?;
            self.live_updaters
                .with_write(|live_updaters| live_updaters.remove(&key));
        }

        let cached_log_id = self
            .bookmarks
            .with_read(|bookmarks| bookmarks.get(&key).and_then(BookmarkState::log_id));
        let Some(notification) = self.update_from_notification(notification) else {
            return Ok(());
        };

        let cached_kind = self
            .bookmarks
            .with_read(|cache| cache.get(&key).map(|state| state.kind));
        let state = self
            .repo
            .warm_from_notification(ctx, &notification, &self.warmers, cached_kind)
            .await?;
        let notification_outcome = if state.is_some() {
            "updated"
        } else {
            "deleted"
        };
        bookmark_notifications::update_cached_bookmark(&self.bookmarks, &notification, state);
        tracing::info!(
            repo = %self.repo.repo_identity().name(),
            bookmark = %key,
            notification_log_id = ?log_id,
            cached_log_id = ?cached_log_id,
            notification_outcome,
            "Applied EventRouter warm bookmark cache notification"
        );
        Ok(())
    }

    // Loop that finds bookmarks that were modified and spawns separate bookmark updaters for them
    pub fn spawn(
        mut self,
        ctx: CoreContext,
        terminate: oneshot::Receiver<()>,
        notify_sync_start: Arc<Notify>,
        notify_sync_complete: Arc<Notify>,
    ) {
        let span = tracing::info_span!("wbc", repo = %self.repo.repo_identity().name());
        let fut = async move {
            tracing::info!("Started warm bookmark cache updater");
            let repo_name: String = self.repo.repo_identity().name().to_string();
            let tailing_enabled = justknobs::eval(
                "scm/mononoke:wbc_update_by_scribe_tailer",
                None,
                Some(&repo_name),
            );
            let mut bookmark_update_subscriber = tailing_enabled
                .then(|| {
                    self.repo
                        .repo_event_publisher
                        .subscribe_for_bookmark_updates(&repo_name)
                        .ok()
                })
                .flatten();
            let infinite_loop = async {
                // Indicates that the sync method was called and is waiting for a sync
                // completion notification
                let mut sync_started = false;
                loop {
                    // Reset the sequence counter for each loop iteration.
                    let ctx = ctx.with_mutated_scuba(|scuba| scuba.with_seq("seq"));
                    let res = self.update(&ctx).await;

                    if let Err(err) = res.as_ref() {
                        #[cfg(fbcode_build)]
                        WBC_INSTRUMENT.observe(MononokeWarmBookmarkCacheStats {
                            event: Some(WarmBookmarkCacheEvent::DiscoverFailure),
                            ..Default::default()
                        });
                        tracing::warn!("failed to update bookmarks {:?}", err);
                    }

                    if sync_started {
                        // Wait for all updaters to finish and notify sync completion
                        if let Err(join_err) = self
                            .updaters_handles
                            .drain()
                            .map(|(_, handle)| handle)
                            .collect::<FuturesUnordered<_>>()
                            .try_collect::<Vec<_>>()
                            .await
                        {
                            tracing::warn!(
                                "failed to join updater tasks when syncing {:?}",
                                join_err
                            );
                        }
                        notify_sync_complete.notify_waiters();
                    }

                    let delay = Duration::from_millis(justknobs::get_as::<u64>(
                        "scm/mononoke:warm_bookmark_cache_poll_interval_ms",
                        None,
                    ));

                    let tailing_enabled = justknobs::eval(
                        "scm/mononoke:wbc_update_by_scribe_tailer",
                        None,
                        Some(&repo_name),
                    );

                    // Receiving a sync notification interrupts sleep/listen and forces
                    // waiting for all updaters to finish in the next iteration
                    let notified = notify_sync_start.notified();

                    if tailing_enabled && let Some(sub) = bookmark_update_subscriber.as_mut() {
                        let receiver_fut = sub.recv();
                        futures::pin_mut!(notified, receiver_fut);
                        match select(notified, receiver_fut).await {
                            future::Either::Left(_) => sync_started = true,
                            future::Either::Right(_) => sync_started = false,
                        }
                    } else {
                        let sleep = tokio::time::sleep(delay);
                        futures::pin_mut!(notified, sleep);
                        match select(notified, sleep).await {
                            future::Either::Left(_) => sync_started = true,
                            future::Either::Right(_) => sync_started = false,
                        }
                    }
                }
            }
            .boxed();

            let _ = select(infinite_loop, terminate).await;

            // Reset the gauge so fb303/ODS stop reporting the last value indefinitely after shutdown.
            STATS::max_staleness_secs.set_value(ctx.fb, 0, (repo_name.clone(),));

            tracing::info!("Stopped warm bookmark cache updater");
        }
        .instrument(span);

        // Fire and forget. This will terminate using the `terminate` receiver.
        std::mem::drop(mononoke::spawn_task(fut));
    }

    #[cfg(fbcode_build)]
    fn spawn_event_router(
        mut self,
        ctx: CoreContext,
        terminate: oneshot::Receiver<()>,
        (notify_sync_start, notify_sync_complete): (Arc<Notify>, Arc<Notify>),
        batching_enabled: bool,
    ) {
        let repo_name = self.repo.repo_identity().name().to_owned();
        let batch_settings = batching_enabled.then(|| {
            let concurrency = justknobs::get(
                "scm/mononoke:wbc_event_router_batch_concurrency",
                Some(&repo_name),
            )
            .clamp(1, 1024) as usize;
            let max_pending_bookmarks = justknobs::get(
                "scm/mononoke:wbc_event_router_max_pending_bookmarks",
                Some(&repo_name),
            )
            .max(1) as usize;
            let delay = Duration::from_millis(justknobs::get_as::<u64>(
                "scm/mononoke:warm_bookmark_cache_poll_interval_ms",
                None,
            ));
            (concurrency, max_pending_bookmarks, delay)
        });
        let span = tracing::info_span!(
            "wbc",
            repo = %self.repo.repo_identity().name(),
            source = "event_router",
        );
        let fut = async move {
            tracing::info!("Started EventRouter warm bookmark cache updater");
            let repo_name: String = self.repo.repo_identity().name().to_string();
            let _activity =
                event_router_coordinator_stats::EventRouterCoordinatorActivity::new(
                    ctx.fb,
                    repo_name.clone(),
                );
            let mut bookmark_event_notification_subscriber = None;
            let infinite_loop = async {
                if let Some(settings) = batch_settings {
                    self.run_event_router_batches(
                        &ctx, (&notify_sync_start, &notify_sync_complete), settings,
                    ).await;
                    return;
                }
                let mut sync_started = false;
                let mut pending_update = None;
                loop {
                    let ctx = ctx.with_mutated_scuba(|scuba| scuba.with_seq("seq"));
                    let res = match pending_update.take() {
                        Some(EventRouterCoordinatorUpdate::Notification(notification)) => {
                            match self.update_inline_from_notification(&ctx, notification).await {
                                Ok(()) => {
                                    STATS::event_router_notification_processed_count.add_value(
                                        1,
                                        (repo_name.clone(),),
                                    );
                                    Ok(())
                                }
                                Err(error) => {
                                    STATS::event_router_notification_failure_count.add_value(
                                        1,
                                        (repo_name.clone(),),
                                    );
                                    STATS::event_router_full_refresh_count.add_value(
                                        1,
                                        (
                                            repo_name.clone(),
                                            "targeted_update_failure".to_owned(),
                                        ),
                                    );
                                    WBC_INSTRUMENT.observe(MononokeWarmBookmarkCacheStats {
                                        event: Some(WarmBookmarkCacheEvent::DiscoverFailure),
                                        ..Default::default()
                                    });
                                    tracing::warn!(
                                        ?error,
                                        "targeted bookmark update failed; falling back to full refresh"
                                    );
                                    self.update(&ctx).await
                                }
                            }
                        }
                        Some(EventRouterCoordinatorUpdate::FullRefresh(reason)) => {
                            STATS::event_router_full_refresh_count.add_value(
                                1,
                                (repo_name.clone(), reason.to_owned()),
                            );
                            self.update(&ctx).await
                        }
                        None => Ok(()),
                    };

                    if let Err(error) = res.as_ref() {
                        WBC_INSTRUMENT.observe(MononokeWarmBookmarkCacheStats {
                            event: Some(WarmBookmarkCacheEvent::DiscoverFailure),
                            ..Default::default()
                        });
                        tracing::warn!(?error, "failed to update bookmarks");
                    }

                    if sync_started {
                        if let Err(join_err) = self
                            .updaters_handles
                            .drain()
                            .map(|(_, handle)| handle)
                            .collect::<FuturesUnordered<_>>()
                            .try_collect::<Vec<_>>()
                            .await
                        {
                            tracing::warn!(
                                "failed to join updater tasks when syncing {:?}",
                                join_err
                            );
                        }
                        notify_sync_complete.notify_waiters();
                    }

                    if bookmark_event_notification_subscriber.is_none() {
                        match self
                            .repo
                            .repo_event_publisher
                            .subscribe_for_bookmark_event_notifications(&repo_name)
                        {
                            Ok(subscriber) => {
                                bookmark_event_notification_subscriber = Some(subscriber);
                            }
                            Err(error) => {
                                STATS::event_router_subscription_failure_count.add_value(
                                    1,
                                    (repo_name.clone(), "subscribe".to_owned()),
                                );
                                tracing::warn!(
                                    ?error,
                                    "failed to subscribe to EventRouter bookmark notifications; retrying after delay"
                                );
                            }
                        }
                    }

                    let notified = notify_sync_start.notified();
                    let mut subscriber_closed = false;
                    if let Some(subscriber) = bookmark_event_notification_subscriber.as_mut() {
                        let notification = subscriber.recv();
                        futures::pin_mut!(notified, notification);
                        match select(notified, notification).await {
                            future::Either::Left(_) => {
                                sync_started = true;
                                pending_update =
                                    Some(EventRouterCoordinatorUpdate::FullRefresh("sync"));
                            }
                            future::Either::Right((notification, _)) => {
                                sync_started = false;
                                match notification {
                                    Ok(notification) => {
                                        pending_update = Some(
                                            EventRouterCoordinatorUpdate::Notification(notification),
                                        );
                                    }
                                    Err(RecvError::Lagged(skipped)) => {
                                        STATS::event_router_subscriber_lag_count
                                            .add_value(1, (repo_name.clone(),));
                                        tracing::warn!(
                                            skipped,
                                            "EventRouter bookmark notification loop lagged"
                                        );
                                        pending_update = Some(
                                            EventRouterCoordinatorUpdate::FullRefresh(
                                                "subscriber_lag",
                                            ),
                                        );
                                    }
                                    Err(RecvError::Closed) => {
                                        STATS::event_router_subscription_failure_count.add_value(
                                            1,
                                            (repo_name.clone(), "closed".to_owned()),
                                        );
                                        tracing::warn!(
                                            "EventRouter bookmark notification loop closed; retrying after delay"
                                        );
                                        subscriber_closed = true;
                                    }
                                }
                            }
                        }
                    } else {
                        let delay = Duration::from_millis(justknobs::get_as::<u64>(
                            "scm/mononoke:warm_bookmark_cache_poll_interval_ms",
                            None,
                        ));
                        let sleep = tokio::time::sleep(delay);
                        futures::pin_mut!(notified, sleep);
                        match select(notified, sleep).await {
                            future::Either::Left(_) => {
                                sync_started = true;
                                pending_update =
                                    Some(EventRouterCoordinatorUpdate::FullRefresh("sync"));
                            }
                            future::Either::Right(_) => sync_started = false,
                        }
                    }
                    if subscriber_closed {
                        bookmark_event_notification_subscriber = None;
                        let delay = Duration::from_millis(justknobs::get_as::<u64>(
                            "scm/mononoke:warm_bookmark_cache_poll_interval_ms",
                            None,
                        ));
                        let notified = notify_sync_start.notified();
                        let sleep = tokio::time::sleep(delay);
                        futures::pin_mut!(notified, sleep);
                        match select(notified, sleep).await {
                            future::Either::Left(_) => {
                                sync_started = true;
                                pending_update =
                                    Some(EventRouterCoordinatorUpdate::FullRefresh("sync"));
                            }
                            future::Either::Right(_) => sync_started = false,
                        }
                    }
                }
            }
            .boxed();

            let _ = select(infinite_loop, terminate).await;
            STATS::max_staleness_secs.set_value(ctx.fb, 0, (repo_name.clone(),));
            tracing::info!("Stopped EventRouter warm bookmark cache updater");
        }
        .instrument(span);

        std::mem::drop(mononoke::spawn_task(fut));
    }
}

fn report_delay_and_remove_finished_updaters(
    ctx: &CoreContext,
    live_updaters: &Arc<RwLock<HashMap<BookmarkKey, BookmarkUpdaterState>>>,
    reponame: &str,
) {
    let mut max_staleness = 0;
    live_updaters.with_write(|live_updaters| {
        let new_updaters = live_updaters
            .drain()
            .filter_map(|(key, value)| {
                use BookmarkUpdaterState::*;
                match value {
                    Started => {}
                    InProgress {
                        oldest_underived_ts,
                    } => {
                        max_staleness =
                            ::std::cmp::max(max_staleness, oldest_underived_ts.since_seconds());
                    }
                    Finished {
                        oldest_underived_ts,
                    } => {
                        if let Some(oldest_underived_ts) = oldest_underived_ts {
                            let staleness_secs = oldest_underived_ts.since_seconds();
                            max_staleness = ::std::cmp::max(max_staleness, staleness_secs);
                        }
                    }
                };

                if value.is_finished() {
                    None
                } else {
                    Some((key, value))
                }
            })
            .collect();
        *live_updaters = new_updaters;
    });

    STATS::max_staleness_secs.set_value(ctx.fb, max_staleness, (reponame.to_owned(),));
    STATS::global_max_staleness_secs.add_value(max_staleness);
    #[cfg(fbcode_build)]
    WBC_INSTRUMENT.observe(MononokeWarmBookmarkCacheStats {
        repo: Some(reponame.to_owned()),
        max_staleness_secs: Some(max_staleness as f64),
        ..Default::default()
    });
}

#[derive(Clone)]
enum BookmarkUpdaterState {
    // Updater has started but it hasn't yet fetched bookmark update log
    Started,
    // Updater is deriving right now
    InProgress {
        oldest_underived_ts: Timestamp,
    },
    // Updater is finished. it might report staleness it it failed.
    Finished {
        oldest_underived_ts: Option<Timestamp>,
    },
}

impl BookmarkUpdaterState {
    fn into_finished(self, bookmark_updater_res: &Result<(), Error>) -> BookmarkUpdaterState {
        use BookmarkUpdaterState::*;

        let oldest_underived_ts = match self {
            // That might happen if by the time updater started everything was already derived
            // or updater failed before it started deriving
            Started => None,
            InProgress {
                oldest_underived_ts,
            } => {
                if bookmark_updater_res.is_err() {
                    Some(oldest_underived_ts)
                } else {
                    None
                }
            }
            // That shouldn't really happen in practice
            // but return anyway
            Finished {
                oldest_underived_ts,
            } => oldest_underived_ts,
        };

        Finished {
            oldest_underived_ts,
        }
    }
}

impl BookmarkUpdaterState {
    fn is_finished(&self) -> bool {
        match self {
            Self::Started | Self::InProgress { .. } => false,
            Self::Finished { .. } => true,
        }
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

async fn single_bookmark_updater(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + BookmarkUpdateLogRef),
    bookmark: &Bookmark,
    bookmarks: &Arc<RwLock<HashMap<BookmarkKey, BookmarkState>>>,
    warmers: &Arc<Vec<Warmer>>,
    mut staleness_reporter: impl FnMut(Timestamp),
) -> Result<(), Error> {
    let cached_log_id = bookmarks.with_read(|bookmarks| {
        bookmarks
            .get(bookmark.key())
            .and_then(BookmarkState::log_id)
    });
    let LatestBookmarkEntries {
        latest_underived,
        latest_log_id,
        derived_entries,
    } = find_latest_derived_and_underived(
        ctx,
        repo.bookmarks(),
        repo.bookmark_update_log(),
        bookmark.key(),
        warmers.as_ref(),
    )
    .await?;

    let update_bookmark = |state: BookmarkState| async move {
        bookmarks.with_write(|bookmarks| {
            let key = bookmark.key().clone();
            bookmarks.insert(key, state)
        });
    };

    let mut bookmark_state = BookmarkState::from_warmers(*bookmark.kind(), warmers);
    if let Some(log_id) = cached_log_id {
        bookmark_state.set_log_id(log_id);
    }

    // Update the state based on what we found in the bookmark log
    let mut found_any_derived = false;
    for (requirement, entry) in derived_entries {
        match entry {
            LatestDerivedBookmarkEntry::Found(maybe_val) => match maybe_val {
                Some((cs_id, _)) => {
                    bookmark_state.set(requirement, cs_id)?;
                    if requirement == WarmerRequirement::AllKinds {
                        found_any_derived = true;
                    }
                }
                None => (), // Nothing to do then
            },
            LatestDerivedBookmarkEntry::NotFound => {
                tracing::warn!(
                    "Haven't found previous derived version of {} for {:?} tags! Will try to derive anyway",
                    bookmark.key(),
                    requirement
                );
            }
        }
    }

    if found_any_derived {
        update_bookmark(bookmark_state.clone()).await;
    } else {
        bookmarks.with_write(|bookmarks| bookmarks.remove(bookmark.key()));
    }

    let LatestUnderivedBookmarkEntry {
        maybe_cs_id,
        maybe_id_ts,
    } = latest_underived;
    if let Some(underived_cs_id) = maybe_cs_id {
        if let Some((_, ts)) = maybe_id_ts {
            // timestamp might not be known if e.g. bookmark has no history.
            // In that case let's not report staleness
            staleness_reporter(ts);
        }

        let bookmark_log_id = maybe_id_ts.as_ref().map(|(id, _)| u64::from(*id));
        let maybe_ts = maybe_id_ts.map(|(_, ts)| ts);

        let ctx = ctx.clone().with_mutated_scuba(|mut scuba| {
            scuba.add("bookmark", bookmark.key().to_string());
            scuba.add("bookmark_log_id", bookmark_log_id);
            scuba.add("top_changeset", underived_cs_id.to_hex().to_string());
            scuba
        });

        ctx.scuba()
            .clone()
            .add("delay_ms", maybe_ts.map(|ts| ts.since_millis()))
            .log_with_msg("Before warming bookmark", None);

        let (stats, res) = warm_all(&ctx, underived_cs_id, warmers).timed().await;

        ctx.scuba()
            .clone()
            .add_future_stats(&stats)
            .log_with_msg("After warming bookmark", None);

        match res {
            Ok(()) => {
                // We just warmed all our warmers, so we can update all tracked requirements
                bookmark_state.set_all_tracked(underived_cs_id);
                if let Some(log_id) = latest_log_id {
                    bookmark_state.set_log_id(log_id);
                }
                update_bookmark(bookmark_state).await;
            }
            Err(err) => {
                tracing::warn!(
                    "failed to derive data for {} while updating {}: {}",
                    underived_cs_id,
                    bookmark.key(),
                    err
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::BookmarkUpdateLog;
    use bookmarks::BookmarkUpdateLogArc;
    use bookmarks::Bookmarks;
    use bookmarks::BookmarksArc;
    use bookmarks::BookmarksMaybeStaleExt;
    use cloned::cloned;
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
    use mononoke_types::RepositoryId;
    use repo_blobstore::RepoBlobstore;
    use repo_derivation_queues::DerivationPriority;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataArc;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_event_publisher::RepoEventPublisher;
    use repo_event_publisher::RepoEventPublisherArc;
    use repo_identity::RepoIdentity;
    use repo_identity::RepoIdentityArc;
    use sql_ext::mononoke_queries;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::bookmark;
    use tests_utils::resolve_cs_id;
    use tokio::time;

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
    );

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
        let mut coordinator = BookmarksCoordinator::new(
            bookmarks.clone(),
            repo.bookmarks()
                .create_subscription(&ctx, Freshness::MostRecent)
                .await?,
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
        coordinator
            .update_inline_from_notification(&ctx, notification)
            .await?;

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
        coordinator
            .update_inline_from_notification(
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
        coordinator
            .update_inline_from_notification(
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

    #[mononoke::fbinit_test]
    async fn test_spawn_bookmarks_coordinator_simple(fb: FacebookInit) -> Result<(), Error> {
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

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        let master = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("somefile", "content")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "master").set_to(master).await?;

        let mut coordinator = BookmarksCoordinator::new(
            bookmarks.clone(),
            repo.bookmarks()
                .create_subscription(&ctx, Freshness::MostRecent)
                .await?,
            repo.bookmarks_arc(),
            repo.bookmark_update_log_arc(),
            repo.repo_identity_arc(),
            repo.repo_event_publisher_arc(),
            warmers,
        );

        let master_book = BookmarkKey::new("master")?;
        let expected_master = test_bookmark_state_all_warmers_with_latest_log_id(
            &ctx,
            &repo,
            "master",
            BookmarkKind::PullDefaultPublishing,
            master,
        )
        .await?;
        update_and_wait_for_bookmark(
            &ctx,
            &mut coordinator,
            &master_book,
            Some(expected_master.clone()),
        )
        .await?;

        bookmark(&ctx, &repo, "master").delete().await?;
        // This check should not be successful in deleting master because it is protected
        update_and_wait_for_bookmark(&ctx, &mut coordinator, &master_book, Some(expected_master))
            .await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_single_bookmarks_coordinator_many_updates(fb: FacebookInit) -> Result<(), Error> {
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

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        tracing::info!("created stack of commits");
        for i in 1..10 {
            let master = CreateCommitContext::new(&ctx, &repo, vec!["master"])
                .add_file(format!("somefile{i}").as_str(), "content")
                .commit()
                .await?;
            tracing::info!("created {}", master);
            bookmark(&ctx, &repo, "master").set_to(master).await?;
        }
        let master_cs_id = resolve_cs_id(&ctx, &repo, "master").await?;
        tracing::info!("created the whole stack of commits");

        let master_book_name = BookmarkKey::new("master")?;
        let master_book = Bookmark::new(
            master_book_name.clone(),
            BookmarkKind::PullDefaultPublishing,
        );
        single_bookmark_updater(&ctx, &repo, &master_book, &bookmarks, &warmers, |_| {}).await?;

        assert_eq!(
            bookmarks.with_read(|bookmarks| bookmarks.get(&master_book_name).cloned()),
            Some(
                test_bookmark_state_all_warmers_with_latest_log_id(
                    &ctx,
                    &repo,
                    "master",
                    BookmarkKind::PullDefaultPublishing,
                    master_cs_id,
                )
                .await?,
            )
        );

        Ok(())
    }

    async fn update_and_wait_for_bookmark(
        ctx: &CoreContext,
        coordinator: &mut BookmarksCoordinator,
        book: &BookmarkKey,
        expected_value: Option<BookmarkState>,
    ) -> Result<(), Error> {
        coordinator.update(ctx).await?;

        let coordinator = &coordinator;

        wait(move || {
            let expected = expected_value.clone();
            async move {
                let val = coordinator
                    .bookmarks
                    .with_read(|bookmarks| bookmarks.get(book).cloned());
                Ok(val == expected)
            }
        })
        .await
    }

    async fn wait<F, Fut>(func: F) -> Result<(), Error>
    where
        F: Fn() -> Fut,
        Fut: futures::future::Future<Output = Result<bool, Error>>,
    {
        let timeout_ms = 4000;
        time::timeout(Duration::from_millis(timeout_ms), async {
            loop {
                if func().await? {
                    break;
                }
                let sleep_ms = 10;
                time::sleep(Duration::from_millis(sleep_ms)).await;
            }

            Ok(())
        })
        .await?
    }

    #[mononoke::fbinit_test]
    async fn test_spawn_bookmarks_coordinator_failing_warmer(
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

        let failing_cs_id = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("failed", "failed")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "failingbook")
            .set_to(failing_cs_id)
            .await?;

        let master = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("somefile", "content")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "master").set_to(master).await?;

        let warmer = Warmer {
            warmer: Box::new({
                cloned!(repo, failing_cs_id);
                move |ctx, cs_id| {
                    if cs_id == failing_cs_id {
                        async { Err(anyhow!("failed")) }.boxed()
                    } else {
                        cloned!(repo);
                        async move {
                            repo.repo_derived_data()
                                .derive::<RootUnodeManifestId>(ctx, cs_id, DerivationPriority::LOW)
                                .await?;
                            Ok(())
                        }
                        .boxed()
                    }
                }
            }),
            is_warm: Box::new({
                cloned!(repo);
                move |ctx, cs_id| {
                    cloned!(repo);
                    async move {
                        let res = repo
                            .repo_derived_data()
                            .fetch_derived::<RootUnodeManifestId>(ctx, cs_id)
                            .await?;
                        Ok(res.is_some())
                    }
                    .boxed()
                }
            }),
            name: "test".to_string(),
            tags: vec![WarmerTag::Hg, WarmerTag::Git],
        };
        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(warmer);
        let warmers = Arc::new(warmers);

        let mut coordinator = BookmarksCoordinator::new(
            bookmarks.clone(),
            repo.bookmarks()
                .create_subscription(&ctx, Freshness::MostRecent)
                .await?,
            repo.bookmarks_arc(),
            repo.bookmark_update_log_arc(),
            repo.repo_identity_arc(),
            repo.repo_event_publisher_arc(),
            warmers,
        );

        let master_book = BookmarkKey::new("master")?;
        update_and_wait_for_bookmark(
            &ctx,
            &mut coordinator,
            &master_book,
            Some(
                test_bookmark_state_all_warmers_with_latest_log_id(
                    &ctx,
                    &repo,
                    "master",
                    BookmarkKind::PullDefaultPublishing,
                    master,
                )
                .await?,
            ),
        )
        .await?;

        // This needs to split a bit because we don't know how long it would take for failing_book
        // to actually show up. :/
        tokio::time::sleep(Duration::from_secs(5)).await;

        let failing_book = BookmarkKey::new("failingbook")?;
        bookmarks.with_read(|bookmarks| assert_eq!(bookmarks.get(&failing_book), None));

        // Now change the warmer and make sure it derives successfully
        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        let mut coordinator = BookmarksCoordinator::new(
            bookmarks.clone(),
            repo.bookmarks()
                .create_subscription(&ctx, Freshness::MostRecent)
                .await?,
            repo.bookmarks_arc(),
            repo.bookmark_update_log_arc(),
            repo.repo_identity_arc(),
            repo.repo_event_publisher_arc(),
            warmers,
        );

        update_and_wait_for_bookmark(
            &ctx,
            &mut coordinator,
            &failing_book,
            Some(
                test_bookmark_state_all_warmers_with_latest_log_id(
                    &ctx,
                    &repo,
                    "failingbook",
                    BookmarkKind::PullDefaultPublishing,
                    failing_cs_id,
                )
                .await?,
            ),
        )
        .await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_spawn_bookmarks_coordinator_check_single_updater(
        fb: FacebookInit,
    ) -> Result<(), Error> {
        let repo: Repo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);

        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(
                &ctx,
                repo.bookmarks()
                    .get(
                        ctx.clone(),
                        &BookmarkKey::new("master")?,
                        bookmarks::Freshness::MostRecent,
                    )
                    .await?
                    .unwrap(),
                DerivationPriority::LOW,
            )
            .await?;

        let derive_sleep_time_ms = 100;
        let how_many_derived = Arc::new(RwLock::new(HashMap::new()));
        let warmer = Warmer {
            warmer: Box::new({
                cloned!(repo, how_many_derived);
                move |ctx, cs_id| {
                    how_many_derived.with_write(|map| {
                        *map.entry(cs_id).or_insert(0) += 1;
                    });
                    cloned!(repo);
                    async move {
                        tokio::time::sleep(Duration::from_millis(derive_sleep_time_ms)).await;
                        repo.repo_derived_data()
                            .derive::<RootUnodeManifestId>(ctx, cs_id, DerivationPriority::LOW)
                            .await?;
                        Ok(())
                    }
                    .boxed()
                }
            }),
            is_warm: Box::new({
                cloned!(repo);
                move |ctx, cs_id| {
                    cloned!(repo);
                    async move {
                        let res = repo
                            .repo_derived_data()
                            .fetch_derived::<RootUnodeManifestId>(ctx, cs_id)
                            .await?
                            .is_some();
                        Ok(res)
                    }
                    .boxed()
                }
            }),
            name: "test".to_string(),
            tags: vec![WarmerTag::Hg, WarmerTag::Git],
        };
        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(warmer);
        let warmers = Arc::new(warmers);

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

        let master = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("somefile", "content")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "master").set_to(master).await?;

        let mut coordinator = BookmarksCoordinator::new(
            bookmarks.clone(),
            repo.bookmarks()
                .create_subscription(&ctx, Freshness::MostRecent)
                .await?,
            repo.bookmarks_arc(),
            repo.bookmark_update_log_arc(),
            repo.repo_identity_arc(),
            repo.repo_event_publisher_arc(),
            warmers.clone(),
        );
        coordinator.update(&ctx).await?;

        // Give it a chance to derive
        wait({
            move || {
                cloned!(ctx, master, warmers);
                async move {
                    let warm_state = get_warm_state(&ctx, master, &warmers).await?;
                    let res: Result<_, Error> = Ok(warm_state.are_all_warm());
                    res
                }
            }
        })
        .await?;

        how_many_derived.with_read(|derived| {
            assert_eq!(derived.get(&master), Some(&1));
        });

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_spawn_bookmarks_coordinator_with_publishing_bookmarks(
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

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        let new_cs_id = CreateCommitContext::new(&ctx, &repo, vec!["master"])
            .add_file("somefile", "content")
            .commit()
            .await?;
        bookmark(&ctx, &repo, "publishing")
            .create_publishing(new_cs_id)
            .await?;

        let mut coordinator = BookmarksCoordinator::new(
            bookmarks.clone(),
            repo.bookmarks()
                .create_subscription(&ctx, Freshness::MostRecent)
                .await?,
            repo.bookmarks_arc(),
            repo.bookmark_update_log_arc(),
            repo.repo_identity_arc(),
            repo.repo_event_publisher_arc(),
            warmers,
        );

        let publishing_book = BookmarkKey::new("publishing")?;
        update_and_wait_for_bookmark(
            &ctx,
            &mut coordinator,
            &publishing_book,
            Some(
                test_bookmark_state_all_warmers_with_latest_log_id(
                    &ctx,
                    &repo,
                    "publishing",
                    BookmarkKind::Publishing,
                    new_cs_id,
                )
                .await?,
            ),
        )
        .await?;

        // Now recreate a bookmark with the same name but different kind
        bookmark(&ctx, &repo, "publishing").delete().await?;
        bookmark(&ctx, &repo, "publishing")
            .set_to(new_cs_id)
            .await?;

        update_and_wait_for_bookmark(
            &ctx,
            &mut coordinator,
            &publishing_book,
            Some(
                test_bookmark_state_all_warmers_with_latest_log_id(
                    &ctx,
                    &repo,
                    "publishing",
                    BookmarkKind::PullDefaultPublishing,
                    new_cs_id,
                )
                .await?,
            ),
        )
        .await?;

        Ok(())
    }

    mononoke_queries! {
        write ClearBookmarkUpdateLog(repo_id: RepositoryId) {
            none,
            "DELETE FROM bookmarks_update_log WHERE repo_id = {repo_id}"
        }
    }

    #[mononoke::fbinit_test]
    async fn test_single_bookmarks_no_history(fb: FacebookInit) -> Result<(), Error> {
        let factory = TestRepoFactory::new(fb)?;
        let repo: Repo = factory.build().await?;
        Linear::init_repo(fb, &repo).await?;
        let ctx = CoreContext::test_mock(fb);

        let bookmarks = Arc::new(RwLock::new(HashMap::new()));

        let mut warmers: Vec<Warmer> = Vec::new();
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            &ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
        let warmers = Arc::new(warmers);

        let master_cs_id = resolve_cs_id(&ctx, &repo, "master").await?;
        warm_all(&ctx, master_cs_id, &warmers).await?;

        let master_book_name = BookmarkKey::new("master")?;
        let master_book = Bookmark::new(
            master_book_name.clone(),
            BookmarkKind::PullDefaultPublishing,
        );

        ClearBookmarkUpdateLog::query(
            &factory.metadata_db().write_connection,
            ctx.sql_query_telemetry(),
            &repo.repo_identity().id(),
        )
        .await?;

        single_bookmark_updater(&ctx, &repo, &master_book, &bookmarks, &warmers, |_| {}).await?;

        assert_eq!(
            bookmarks.with_read(|bookmarks| bookmarks.get(&master_book_name).cloned()),
            Some(test_bookmark_state_all_warmers(
                BookmarkKind::PullDefaultPublishing,
                master_cs_id,
            ))
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
}
