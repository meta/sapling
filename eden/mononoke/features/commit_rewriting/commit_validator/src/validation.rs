/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use anyhow::format_err;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogEntry;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarksRef;
use cloned::cloned;
use commit_graph::CommitGraphArc;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use cross_repo_sync::CommitSyncOutcome;
use cross_repo_sync::Large;
use cross_repo_sync::Small;
use cross_repo_sync::Source;
use cross_repo_sync::Target;
use cross_repo_sync::get_commit_sync_outcome;
use cross_repo_sync::report_different;
use futures::TryFutureExt;
use futures::future;
use futures::future::try_join_all;
use futures::stream;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_stats::TimedFutureExt;
use live_commit_sync_config::CfgrLiveCommitSyncConfig;
use live_commit_sync_config::LiveCommitSyncConfig;
use manifest::Diff;
use manifest::Entry;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_types::FileType;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::sharded_augmented_manifest::find_hg_entry;
use mercurial_types::sharded_augmented_manifest::hg_manifest_diff;
use mercurial_types::sharded_augmented_manifest::list_all_hg_entries;
use metaconfig_types::CommitSyncConfigVersion;
use metaconfig_types::CommitSyncDirection;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::RepositoryId;
use movers::Mover;
use movers::get_movers;
use ref_cast::RefCast;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use scuba_ext::MononokeScubaSampleBuilder;
use stats::prelude::*;
use synced_commit_mapping::SqlSyncedCommitMapping;
use synced_commit_mapping::SyncedCommitMapping;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::repo::Repo;
use crate::reporting::log_validation_result_to_scuba;
use crate::tail::QueueSize;

type LargeBookmarkKey = Large<BookmarkKey>;
type PathToFileNodeIdMapping = HashMap<NonRootMPath, (FileType, HgFileNodeId)>;

define_stats! {
    prefix = "mononoke.commit_validation";
    // @lint-ignore SPELL
    bizzare_boookmark_move: timeseries(Rate, Sum),
}

/// A struct, representing a single "validation entry":
/// a single commit in the `BookmarkUpdateLogEntry`
#[derive(Clone, PartialEq, Eq)]
pub struct EntryCommitId {
    /// A `BookmarkUpdateLogEntry` id, which introduced this commit
    pub bookmarks_update_log_entry_id: BookmarkUpdateLogId,
    /// An index of this commit in the "parent" `BookmarkUpdateLogEntry`
    commit_in_entry: i64,
    /// Total number of commits, introduced by the `BookmarkUpdateLogEntry`
    /// Needed to provide convenient logging and verify if commit is the
    /// last in the `BookmarkUpdateLogEntry`
    total_commits_in_entry: usize,
}

impl EntryCommitId {
    pub fn last_commit_for_bookmark_move(&self) -> bool {
        self.commit_in_entry == (self.total_commits_in_entry as i64) - 1
    }
}

impl Debug for EntryCommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Entry {} ({}/{})",
            self.bookmarks_update_log_entry_id,
            // 1-indexing to be clear to humans
            self.commit_in_entry + 1,
            self.total_commits_in_entry
        )
    }
}

/// Enum, representing a change to a single `NonRootMPath` in
/// a `FullManifestDiff`
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum FilenodeDiffPayload {
    Added(FileType, HgFileNodeId),
    Removed,
    ChangedTo(FileType, HgFileNodeId),
}

/// A unit of change in a `FullManifestDiff`
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FilenodeDiff {
    /// An `NonRootMPath`, being changed
    mpath: NonRootMPath,
    /// A change, happening to the `mpath`
    payload: FilenodeDiffPayload,
}

impl FilenodeDiff {
    fn new(mpath: NonRootMPath, payload: FilenodeDiffPayload) -> Self {
        Self { mpath, payload }
    }

    /// Build `FilenodeDiff` from `manifest::Diff`
    fn from_diff(diff: Diff<Entry<HgManifestId, (FileType, HgFileNodeId)>>) -> Option<Self> {
        use Diff::*;
        use Entry::*;
        match diff {
            // We don't care about tree diffs!
            Added(_, Tree(_)) | Removed(_, Tree(_)) | Changed(_, _, Tree(_)) => None,
            Added(mpath, Leaf((file_type, hg_filenode_id))) => {
                // We don't care about repo root diffs!
                if mpath.is_root() {
                    None
                } else {
                    // Safe to unwrap since the path is not root
                    Some(Self::new(
                        mpath.try_into().unwrap(),
                        FilenodeDiffPayload::Added(file_type, hg_filenode_id),
                    ))
                }
            }
            Removed(mpath, Leaf(_)) => {
                // We don't care about repo root diffs!
                if mpath.is_root() {
                    None
                } else {
                    // Safe to unwrap since the path is not root
                    Some(Self::new(
                        mpath.try_into().unwrap(),
                        FilenodeDiffPayload::Removed,
                    ))
                }
            }
            Changed(mpath, _, Leaf((file_type, hg_filenode_id))) => {
                // We don't care about repo root diffs!
                if mpath.is_root() {
                    None
                } else {
                    Some(Self::new(
                        mpath.try_into().unwrap(), // Safe to unwrap since the path is not root
                        FilenodeDiffPayload::ChangedTo(file_type, hg_filenode_id),
                    ))
                }
            }
        }
    }

    fn from_added_file(added_file: (NonRootMPath, (FileType, HgFileNodeId))) -> Self {
        let (mpath, (file_type, hg_filenode_id)) = added_file;
        let payload = FilenodeDiffPayload::Added(file_type, hg_filenode_id);
        Self { mpath, payload }
    }

    pub fn apply_mover(self, mover: &dyn Mover) -> Result<Option<Self>, Error> {
        let Self { mpath, payload } = self;
        mover.move_path(&mpath).map(|maybe_moved_mpath| {
            maybe_moved_mpath.map(|moved_mpath| Self::new(moved_mpath, payload))
        })
    }

    pub fn as_tuple(&self) -> (&NonRootMPath, &FilenodeDiffPayload) {
        let Self { mpath, payload } = self;
        (mpath, payload)
    }
}

/// A set of changes between two commit manifests, computed
/// by running `ManifestOps::Diff`
type FullManifestDiff = HashSet<FilenodeDiff>;

/// An auxiliary struct, containing helper methods for sync
/// validation between a large repo and a single small repo
#[derive(Clone)]
struct ValidationHelper {
    large_repo: Large<Repo>,
    small_repo: Small<Repo>,
    scuba_sample: MononokeScubaSampleBuilder,
    live_commit_sync_config: CfgrLiveCommitSyncConfig,
}

impl ValidationHelper {
    fn new(
        large_repo: Large<Repo>,
        small_repo: Small<Repo>,
        scuba_sample: MononokeScubaSampleBuilder,
        live_commit_sync_config: CfgrLiveCommitSyncConfig,
    ) -> Self {
        Self {
            large_repo,
            small_repo,
            scuba_sample,
            live_commit_sync_config,
        }
    }

    async fn get_synced_commit(
        &self,
        ctx: &CoreContext,
        hash: Large<ChangesetId>,
        mapping: &SqlSyncedCommitMapping,
    ) -> Result<Option<(Small<ChangesetId>, CommitSyncConfigVersion)>, Error> {
        let large_repo_id = self.large_repo.0.repo_identity().id();
        let small_repo_id = self.small_repo.0.repo_identity().id();

        let maybe_commit_sync_outcome: Option<CommitSyncOutcome> = get_commit_sync_outcome(
            ctx,
            Source(large_repo_id),
            Target(small_repo_id),
            Source(hash.0),
            mapping,
            CommitSyncDirection::Backwards,
            Arc::new(self.live_commit_sync_config.clone()),
        )
        .await?;

        use CommitSyncOutcome::*;
        Ok(match maybe_commit_sync_outcome {
            None | Some(NotSyncCandidate(_)) | Some(EquivalentWorkingCopyAncestor(_, _)) => None,
            Some(RewrittenAs(cs_id, version_name)) => Some((Small(cs_id), version_name)),
        })
    }

    fn move_full_manifest_diff_large_to_small(
        &self,
        full_manifest_diff: Large<FullManifestDiff>,
        large_to_small_mover: &dyn Mover,
    ) -> Result<Large<FullManifestDiff>, Error> {
        let moved_fmd: Result<FullManifestDiff, Error> = full_manifest_diff
            .0
            .into_iter()
            .flat_map(|filenode_diff| filenode_diff.apply_mover(large_to_small_mover).transpose())
            .collect();
        Ok(Large(moved_fmd?))
    }

    /// Check if `payload` is a noop change against p1 of the current commit
    ///
    /// Because of differences of how Mononoke and Mercurial generate filenodes
    /// for merge commits, it is possible that one backend reuses an existing
    /// `HgFilenodeId`, while the other backend generates a new one, while using
    /// the existing one as a parent. A practical case when this may happen arises
    /// from the fact that during a push Mononoke preserves a provided filenode,
    /// while during a x-repo sync it generates a new one.
    /// So the following situation is possible:
    ///
    /// A    ->  A'
    /// |\   ->  | \
    /// B C  ->  B' C'
    ///
    /// -> represents a x-repo sync
    /// A and B share the same `HgFilenodeId` for some `NonRootMPath`, while
    /// A' and B' have different `HgFilenodeId`s. At the same time, in all
    /// cases the actual `ContentId` and `FileType` is the same!
    async fn is_true_change(
        &self,
        ctx: &CoreContext,
        repo: &Repo,
        p1_root_mf_id: &HgManifestId,
        mpath: &NonRootMPath,
        payload: &FilenodeDiffPayload,
    ) -> Result<bool, Error> {
        debug!(
            "Checking if a change to {:?}'s filenode in {} ({:?}) relects a true change",
            mpath,
            repo.repo_identity().name(),
            payload
        );

        use FilenodeDiffPayload::*;
        match payload {
            Added(_, _) | Removed => Ok(true),
            ChangedTo(new_file_type, new_filenode_id) => {
                let maybe_entry_in_p1 = find_hg_entry(
                    ctx,
                    repo.repo_blobstore().clone(),
                    repo.repo_identity().name(),
                    *p1_root_mf_id,
                    mpath.clone().into(),
                )
                .await?;

                let (p1_file_type, p1_filenode_id) = match maybe_entry_in_p1 {
                    None => {
                        // p1 does not even have an entry at this path, definitely
                        // something fishy (we really should not have gotten ChangedTo
                        // in the first place).
                        return Err(format_err!(
                            "parent's manifest lacks a file at {mpath:?}, but we were told the diff is Changed, not Added"
                        ));
                    }
                    Some(Entry::Tree(_)) => {
                        // p1 has this path as a directory, it's a true change
                        debug!(
                            "change at {:?} in {} was from a tree to a file, so a true change",
                            mpath,
                            repo.repo_identity().name()
                        );
                        return Ok(true);
                    }
                    Some(Entry::Leaf((p1_file_type, p1_filenode_id))) => {
                        (p1_file_type, p1_filenode_id)
                    }
                };

                if p1_file_type != *new_file_type {
                    debug!(
                        "change at {:?} in {} was to a file type: {:?}->{:?}",
                        mpath,
                        repo.repo_identity().name(),
                        p1_file_type,
                        new_file_type,
                    );
                    return Ok(true);
                }

                let (p1_content_id, new_content_id) = try_join!(
                    async {
                        let e = p1_filenode_id.load(ctx, repo.repo_blobstore()).await?;
                        Result::<_, Error>::Ok(e.content_id())
                    },
                    async {
                        let e = new_filenode_id.load(ctx, repo.repo_blobstore()).await?;
                        Result::<_, Error>::Ok(e.content_id())
                    }
                )?;

                if p1_content_id != new_content_id {
                    debug!(
                        "change at {:?} in {} resolved to different ContentIds: {:?}->{:?}",
                        mpath,
                        repo.repo_identity().name(),
                        p1_content_id,
                        new_content_id,
                    );

                    Ok(true)
                } else {
                    debug!(
                        "change at {:?} in {} resolved to identical ContentIds",
                        mpath,
                        repo.repo_identity().name(),
                    );

                    Ok(false)
                }
            }
        }
    }

    /// Remove noop `FilenodeDiffPayload` changes from `paths_and_payloads`
    /// See the docstring of `is_true_change` for more info.
    async fn filter_out_noop_filenode_id_changes(
        &self,
        ctx: &CoreContext,
        repo: &Repo,
        cs_id: &ChangesetId,
        paths_and_payloads: Vec<(NonRootMPath, &FilenodeDiffPayload)>,
    ) -> Result<Vec<NonRootMPath>, Error> {
        let maybe_p1 = repo
            .commit_graph()
            .changeset_parents(ctx, cs_id.clone())
            .await?
            .first()
            .cloned();

        let p1 = match maybe_p1 {
            Some(p1) => p1,
            None => {
                // `cs_id` is a root commit, such commits cannot have "fake" filenode changes
                // Just return all paths
                return Ok(paths_and_payloads.into_iter().map(|(mp, _)| mp).collect());
            }
        };

        let p1_root_mf_id = fetch_root_mf_id(ctx, repo, p1).await?;

        // Note: this loop will only ever be entered during the cases when
        // two repos have non-empty symmetric difference of their `FullManifestDiff`s
        // In other words: extremely rarely. Therefore keeping it sequential is fine
        // for now. We can improve it if it ever becomes a problem.
        let mut ret = Vec::new();
        for (mpath, payload) in paths_and_payloads {
            if self
                .is_true_change(ctx, repo, &p1_root_mf_id, &mpath, payload)
                .await?
            {
                ret.push(mpath);
            } else {
                info!(
                    "Skipping a change to {:?} as the filenode change is deemed false",
                    mpath
                );
            }
        }

        Ok(ret)
    }

    async fn filter_out_large_noop_filenode_id_changes(
        &self,
        ctx: &CoreContext,
        large_cs_id: &Large<ChangesetId>,
        paths_and_payloads: Vec<(Large<NonRootMPath>, Large<&FilenodeDiffPayload>)>,
    ) -> Result<Vec<Large<NonRootMPath>>, Error> {
        let v: Vec<_> = self
            .filter_out_noop_filenode_id_changes(
                ctx,
                &self.large_repo.0,
                &large_cs_id.0,
                paths_and_payloads
                    .into_iter()
                    .map(|(mp, pld)| (mp.0, pld.0))
                    .collect(),
            )
            .await?
            .into_iter()
            .map(Large)
            .collect();

        Ok(v)
    }

    async fn filter_out_small_noop_filenode_id_changes(
        &self,
        ctx: &CoreContext,
        small_cs_id: &Small<ChangesetId>,
        paths_and_payloads: Vec<(Small<NonRootMPath>, Small<&FilenodeDiffPayload>)>,
    ) -> Result<Vec<Small<NonRootMPath>>, Error> {
        let v: Vec<_> = self
            .filter_out_noop_filenode_id_changes(
                ctx,
                &self.small_repo.0,
                &small_cs_id.0,
                paths_and_payloads
                    .into_iter()
                    .map(|(mp, pld)| (mp.0, pld.0))
                    .collect(),
            )
            .await?
            .into_iter()
            .map(Small)
            .collect();

        Ok(v)
    }
}

/// An auxiliary struct, containing helper methods for sync
/// validation a large repo and potentially multiple small repos
#[derive(Clone)]
pub struct ValidationHelpers {
    large_repo: Large<Repo>,
    helpers: HashMap<Small<RepositoryId>, ValidationHelper>,
    /// The "master" bookmark in the large repo. This is needed when
    /// we are unfolding an entry, which creates a new bookmark. Such entry
    /// does not have `from_changeset_id`, so instead we using the `to_changeset_id % master`
    /// revset to unfold an entry.
    large_repo_master_bookmark: BookmarkKey,
    live_commit_sync_config: CfgrLiveCommitSyncConfig,
    mapping: SqlSyncedCommitMapping,
}

impl ValidationHelpers {
    pub fn new(
        large_repo: Repo,
        helpers: HashMap<RepositoryId, (Large<Repo>, Small<Repo>, MononokeScubaSampleBuilder)>,
        large_repo_master_bookmark: BookmarkKey,
        mapping: SqlSyncedCommitMapping,
        live_commit_sync_config: CfgrLiveCommitSyncConfig,
    ) -> Self {
        Self {
            large_repo: Large(large_repo),
            helpers: helpers
                .into_iter()
                .map(|(repo_id, (large_repo, small_repo, scuba_sample))| {
                    (
                        Small(repo_id),
                        ValidationHelper::new(
                            large_repo,
                            small_repo,
                            scuba_sample,
                            live_commit_sync_config.clone(),
                        ),
                    )
                })
                .collect(),
            large_repo_master_bookmark,
            live_commit_sync_config,
            mapping,
        }
    }

    fn get_helper(&self, repo_id: &Small<RepositoryId>) -> Result<&ValidationHelper, Error> {
        self.helpers
            .get(repo_id)
            .ok_or_else(|| format_err!("Repo {repo_id} is not present in ValidationHelpers"))
    }

    fn get_small_repo(&self, repo_id: &Small<RepositoryId>) -> Result<&Small<Repo>, Error> {
        let helper = self.get_helper(repo_id)?;
        Ok(&helper.small_repo)
    }

    // For a given commit, find all repos into which this commit was rewritten,
    // the resulting `ChangesetId` values, and the renamed bookmark
    async fn get_synced_commits(
        &self,
        ctx: &CoreContext,
        hash: Large<ChangesetId>,
    ) -> Result<HashMap<Small<RepositoryId>, (Small<ChangesetId>, CommitSyncConfigVersion)>, Error>
    {
        let commit_sync_outcomes = try_join_all(self.helpers.iter().map(|(repo_id, helper)| {
            let mapping = &self.mapping;
            async move {
                let maybe_synced_commit =
                    helper.get_synced_commit(ctx, hash.clone(), mapping).await?;

                Result::<_, Error>::Ok(
                    maybe_synced_commit.map(|synced_commit| (repo_id.clone(), synced_commit)),
                )
            }
        }))
        .await?
        .into_iter()
        .filter_map(std::convert::identity)
        .collect();

        debug!(
            "Commit {} is rewritten as follows: {:?}",
            hash, commit_sync_outcomes
        );
        Ok(commit_sync_outcomes)
    }

    async fn get_root_full_manifest_diff(
        ctx: &CoreContext,
        repo: &Repo,
        root_mf_id: HgManifestId,
    ) -> Result<FullManifestDiff, Error> {
        let all_files = list_all_filenode_ids(ctx, repo, root_mf_id).await?;
        Ok(all_files
            .into_iter()
            .map(FilenodeDiff::from_added_file)
            .collect())
    }

    /// Produce a full manifest diff between `cs_id` and its first parent
    async fn get_full_manifest_diff(
        ctx: &CoreContext,
        repo: &Repo,
        cs_id: &ChangesetId,
    ) -> Result<FullManifestDiff, Error> {
        let cs_root_mf_id_fut = fetch_root_mf_id(ctx, repo, cs_id.clone());
        let maybe_p1 = repo
            .commit_graph()
            .changeset_parents(ctx, cs_id.clone())
            .await?
            .first()
            .cloned();

        let p1 = match maybe_p1 {
            Some(p1) => p1,
            None => {
                info!("{} is a root cs. Grabbing its entire manifest", cs_id);
                return Self::get_root_full_manifest_diff(ctx, repo, cs_root_mf_id_fut.await?)
                    .await;
            }
        };

        let p1_root_mf_id_fut = fetch_root_mf_id(ctx, repo, p1);
        let (cs_root_mf_id, p1_root_mf_id): (HgManifestId, HgManifestId) =
            try_join!(cs_root_mf_id_fut, p1_root_mf_id_fut)?;

        let r: Vec<Result<_, Error>> = hg_manifest_diff(
            ctx,
            repo.repo_blobstore().clone(),
            repo.repo_identity().name(),
            p1_root_mf_id,
            cs_root_mf_id,
        )
        .filter_map(|diff| async move { diff.map(FilenodeDiff::from_diff).transpose() })
        .collect()
        .await;

        r.into_iter().collect()
    }

    async fn get_large_repo_full_manifest_diff(
        &self,
        ctx: &CoreContext,
        cs_id: &Large<ChangesetId>,
    ) -> Result<Large<FullManifestDiff>, Error> {
        Self::get_full_manifest_diff(ctx, &self.large_repo, &cs_id.0)
            .await
            .map(Large)
    }

    async fn get_small_repos_full_manifest_diffs(
        &self,
        ctx: &CoreContext,
        repo_cs_ids: HashMap<Small<RepositoryId>, (Small<ChangesetId>, CommitSyncConfigVersion)>,
    ) -> Result<
        HashMap<
            Small<RepositoryId>,
            (
                Small<ChangesetId>,
                Small<FullManifestDiff>,
                CommitSyncConfigVersion,
            ),
        >,
        Error,
    > {
        let full_manifest_diff_futures = repo_cs_ids.into_iter().map(
            |(small_repo_id, (small_cs_id, version_name))| async move {
                let small_repo = self.get_small_repo(&small_repo_id)?;
                let full_manifest_diff =
                    Self::get_full_manifest_diff(ctx, &small_repo.0, &small_cs_id.0).await?;
                Result::<_, Error>::Ok((
                    small_repo_id,
                    (small_cs_id, Small(full_manifest_diff), version_name),
                ))
            },
        );

        let full_manifest_diff: Vec<(
            Small<RepositoryId>,
            (
                Small<ChangesetId>,
                Small<FullManifestDiff>,
                CommitSyncConfigVersion,
            ),
        )> = try_join_all(full_manifest_diff_futures).await?;

        Ok(full_manifest_diff.into_iter().collect())
    }

    async fn get_large_repo_master(&self, ctx: CoreContext) -> Result<ChangesetId, Error> {
        let maybe_cs_id = self
            .large_repo
            .bookmarks()
            .get(
                ctx,
                &self.large_repo_master_bookmark,
                bookmarks::Freshness::MostRecent,
            )
            .await?;
        maybe_cs_id.ok_or_else(|| format_err!("No master in the large repo"))
    }

    // First returned mover is small to large, second is large to small
    async fn create_movers(
        &self,
        small_repo_id: &Small<RepositoryId>,
        version_name: &CommitSyncConfigVersion,
    ) -> Result<(Arc<dyn Mover>, Arc<dyn Mover>), Error> {
        let commit_sync_config = self
            .live_commit_sync_config
            .get_commit_sync_config_by_version(self.large_repo.repo_identity().id(), version_name)
            .await?;

        let movers = get_movers(
            &commit_sync_config,
            small_repo_id.0,
            CommitSyncDirection::Forward,
        )?;
        Ok((movers.mover, movers.reverse_mover))
    }
}

/// This represents a single commit move after `BookmarkUpdateLogEntry` is unfolded
#[derive(Debug)]
pub struct CommitEntry {
    entry_id: EntryCommitId,
    bookmark_name: BookmarkKey,
    cs_id: ChangesetId,
    queue_size: QueueSize,
}

/// Given a `BookmarkUpdateLogEntry`, create a stream of `CommitEntry` objects,
/// one for each commit, introduced by this entry
/// This will return an empty stream for entry, which deletes a bookmark.
/// For entry, which creates a bookmark, an equivalent of a revset `new_book % master`
/// will be returned.
pub async fn unfold_bookmarks_update_log_entry(
    ctx: &CoreContext,
    (entry, queue_size): (BookmarkUpdateLogEntry, QueueSize),
    validation_helpers: &ValidationHelpers,
) -> Result<impl Stream<Item = Result<CommitEntry, Error>> + use<>, Error> {
    let bookmarks_update_log_entry_id = entry.id;
    let is_master_entry = entry.bookmark_name == validation_helpers.large_repo_master_bookmark;
    let master_cs_id = validation_helpers
        .get_large_repo_master(ctx.clone())
        .await?;

    info!(
        "BookmarkUpdateLogEntry {} will be expanded into commits",
        bookmarks_update_log_entry_id,
    );

    // Normally a single entry would resolve to just a few commits.
    // Even if we imagine a case, where a single entry will produce
    // a stream of a million commits (merge of a new repo or smth like this)
    // we are talking about <200bytes * 1M ~= 200Mb, which it not that big
    // of a deal
    let mut collected = match (
        entry.from_changeset_id.clone(),
        entry.to_changeset_id.clone(),
    ) {
        (_, None) => {
            // Entry deletes a bookmark. Nothing to validate
            vec![]
        }
        (None, Some(to_cs_id)) => {
            // Entry creates a bookmark (or it has been created by the
            // mononoke_admin in the original repo).
            // In any case, out best bet is to build up all
            // commits since LCA(to_cs_id, master)

            if is_master_entry {
                // A bizarre case, when a master bookmark is created.
                // I do not think we will ever observe this in
                // practice, but for completeness sake, let's just make sure
                // we verify `to_cs_id` itself. If we decided to just use
                // the revset from below, it would've excluded `to_cs_id`
                // as it is likely to be an ancestor of master.
                // @lint-ignore SPELL
                STATS::bizzare_boookmark_move.add_value(1);
                vec![Ok(to_cs_id)]
            } else {
                info!(
                    "[{}] Creation of a new bookmark at {}, slow path.",
                    bookmarks_update_log_entry_id, to_cs_id
                );
                // This might be slow. If too many bookmarks are being created, it can be optimised
                // or we could just check to_cs_id as a best effort.
                validation_helpers
                    .large_repo
                    .commit_graph()
                    .ancestors_difference_stream(ctx, vec![to_cs_id], vec![master_cs_id])
                    .await?
                    .collect()
                    .await
            }
        }
        (Some(from_cs_id), Some(to_cs_id)) => {
            if !validation_helpers
                .large_repo
                .commit_graph()
                .is_ancestor(ctx, from_cs_id, to_cs_id)
                .await?
            {
                info!(
                    "[{}] {} -> {} not a forward move, slow path.",
                    bookmarks_update_log_entry_id, from_cs_id, to_cs_id
                );
                // This might be slow. If too many bookmarks are being non-FF moved, it can be optimised
                // or we could just check to_cs_id as a best effort.
                validation_helpers
                    .large_repo
                    .commit_graph()
                    .ancestors_difference_stream(
                        ctx,
                        vec![to_cs_id],
                        if is_master_entry {
                            vec![from_cs_id]
                        } else {
                            // Skip ancestors of master since validating master bookmark moves covers them.
                            vec![from_cs_id, master_cs_id]
                        },
                    )
                    .await?
                    .collect()
                    .await
            } else {
                validation_helpers
                    .large_repo
                    .commit_graph()
                    .range_stream(ctx, from_cs_id, to_cs_id)
                    .await?
                    // Drop from_cs
                    .skip(1)
                    .map(Ok)
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .rev()
                    .collect()
            }
        }
    };
    // Changesets are listed top to bottom, let's reverse
    collected.reverse();

    let total_commits_in_entry = collected.len();
    info!(
        "BookmarkUpdateLogEntry {} has been expanded into {} commits",
        bookmarks_update_log_entry_id, total_commits_in_entry
    );

    let bookmark_name = entry.bookmark_name;
    Ok(stream::iter(collected).map_ok({
        cloned!(bookmark_name);
        let mut commit_in_entry = 0;
        move |cs_id| {
            commit_in_entry += 1;

            let entry_id = EntryCommitId {
                bookmarks_update_log_entry_id,
                commit_in_entry: commit_in_entry - 1,
                total_commits_in_entry,
            };

            CommitEntry {
                entry_id,
                bookmark_name: bookmark_name.clone(),
                cs_id,
                queue_size,
            }
        }
    }))
}

/// A preprocessed-entry struct, which has enough data for 2 purposes:
/// - to decide if entry `id` needs to be skipped
/// - to create an `EntryPreparedForValidation`
#[derive(Debug)]
pub struct CommitEntryWithSmallReposMapped {
    entry_id: EntryCommitId,
    bookmark_name: LargeBookmarkKey,
    cs_id: Large<ChangesetId>,
    small_repo_cs_ids: HashMap<Small<RepositoryId>, (Small<ChangesetId>, CommitSyncConfigVersion)>,
    queue_size: QueueSize,
}

pub async fn get_entry_with_small_repo_mappings(
    ctx: &CoreContext,
    entry: CommitEntry,
    validation_helpers: &ValidationHelpers,
) -> Result<Option<CommitEntryWithSmallReposMapped>, Error> {
    info!(
        "Mapping small cs_ids for entry {:?}; book: {}; to_cs_id: {:?}; remaining queue: {}",
        entry.entry_id, entry.bookmark_name, entry.cs_id, entry.queue_size.0,
    );
    let cs_id = Large(entry.cs_id.clone());

    let small_repo_cs_ids: HashMap<
        Small<RepositoryId>,
        (Small<ChangesetId>, CommitSyncConfigVersion),
    > = validation_helpers
        .get_synced_commits(ctx, cs_id.clone())
        .await?;

    Ok(Some(CommitEntryWithSmallReposMapped {
        entry_id: entry.entry_id.clone(),
        bookmark_name: Large(entry.bookmark_name),
        cs_id,
        small_repo_cs_ids,
        queue_size: entry.queue_size,
    }))
}

#[derive(Debug)]
pub struct EntryPreparedForValidation {
    pub entry_id: EntryCommitId,
    cs_id: Large<ChangesetId>,
    large_repo_full_manifest_diff: Large<FullManifestDiff>,
    small_repo_full_manifest_diffs: HashMap<
        Small<RepositoryId>,
        (
            Small<ChangesetId>,
            Small<FullManifestDiff>,
            CommitSyncConfigVersion,
        ),
    >,
    queue_size: QueueSize,
    preparation_duration: Duration,
}

pub async fn prepare_entry(
    ctx: &CoreContext,
    entry_with_small_repo_mappings: CommitEntryWithSmallReposMapped,
    validation_helpers: &ValidationHelpers,
) -> Result<Option<EntryPreparedForValidation>, Error> {
    let CommitEntryWithSmallReposMapped {
        entry_id,
        bookmark_name,
        cs_id,
        small_repo_cs_ids,
        queue_size,
    } = entry_with_small_repo_mappings;
    let cs_id = cs_id.clone();

    let before_preparation = std::time::Instant::now();
    info!(
        "Preparing entry {:?}; book: {}; cs_id: {:?}; remaining queue: {}",
        entry_id, bookmark_name, cs_id, queue_size.0,
    );

    // Note: executing the following two async operations  concurrently
    // does not really matter as we are already executing multiple
    // entry preparations at the same time
    let (large_repo_full_manifest_diff, small_repo_full_manifest_diffs) = try_join!(
        validation_helpers.get_large_repo_full_manifest_diff(ctx, &cs_id),
        validation_helpers.get_small_repos_full_manifest_diffs(ctx, small_repo_cs_ids)
    )?;

    Ok(Some(EntryPreparedForValidation {
        entry_id,
        cs_id,
        large_repo_full_manifest_diff,
        small_repo_full_manifest_diffs,
        queue_size,
        preparation_duration: before_preparation.elapsed(),
    }))
}

/// Validate that parents of a changeset in a small repo are
/// ancestors of it's equivalent in the large repo
async fn validate_topological_order<'a, R: RepoIdentityRef + CommitGraphRef + CommitGraphArc>(
    ctx: &'a CoreContext,
    large_repo: &'a Large<R>,
    large_cs_id: Large<ChangesetId>,
    small_repo: &'a Small<R>,
    small_cs_id: Small<ChangesetId>,
    mapping: Arc<dyn SyncedCommitMapping>,
    live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
) -> Result<(), Error> {
    debug!(
        "validating topological order for {}<->{}",
        large_cs_id, small_cs_id
    );
    let small_repo_id = small_repo.0.repo_identity().id();
    let large_repo_id = large_repo.0.repo_identity().id();

    let small_parents = small_repo
        .0
        .commit_graph()
        .changeset_parents(ctx, small_cs_id.0.clone())
        .await?;

    let remapped_small_parents: Vec<(ChangesetId, ChangesetId)> =
        try_join_all(small_parents.into_iter().map(|small_parent| {
            cloned!(ctx, live_commit_sync_config, mapping);
            async move {
                let maybe_commit_sync_outcome = get_commit_sync_outcome(
                    &ctx,
                    Source(small_repo_id),
                    Target(large_repo_id),
                    Source(small_parent),
                    &mapping,
                    CommitSyncDirection::Forward,
                    live_commit_sync_config,
                )
                .await?;

                let commit_sync_outcome = maybe_commit_sync_outcome.ok_or_else(|| {
                    format_err!(
                        "Unexpectedly missing CommitSyncOutcome for {small_parent} in {small_repo_id}->{large_repo_id}",
                    )
                })?;

                use CommitSyncOutcome::*;
                let remapping_of_small_parent = match commit_sync_outcome {
                    RewrittenAs(cs_id, _) | EquivalentWorkingCopyAncestor(cs_id, _) => cs_id,
                    NotSyncCandidate(_) => {
                        return Err(format_err!(
                            "Parent of synced {small_cs_id} is NotSyncCandidate in {small_repo_id}->{large_repo_id}",
                        ));
                    }
                };
                Ok((small_parent, remapping_of_small_parent))
            }
        }))
        .await?;

    let large_repo_commit_graph = large_repo.0.commit_graph_arc();
    try_join_all(remapped_small_parents.into_iter().map(
        |(small_parent, remapping_of_small_parent)| {
            cloned!(ctx, large_repo_commit_graph);
            async move {
                if !large_repo_commit_graph
                    .is_ancestor(&ctx, remapping_of_small_parent, large_cs_id.0.clone())
                    .await?
                {
                    Err(format_err!(
                        "{remapping_of_small_parent} (remapping of parent {small_parent} of {small_cs_id} in {small_repo_id}) is not an ancestor of {large_cs_id} in {large_repo_id}",
                    ))
                } else {
                    Ok(())
                }
            }
        },
    ))
    .await?;

    debug!(
        "done validating topological order for {}<->{}",
        large_cs_id, small_cs_id
    );
    Ok(())
}

/// Given changes to equivalent `NonRootMPath`s in two synced repos,
/// return the differences between the changes
fn compare_diffs_at_mpath(
    mpath: &NonRootMPath,
    small_payload: Small<&FilenodeDiffPayload>,
    large_payload: Large<&FilenodeDiffPayload>,
) -> (
    Option<(NonRootMPath, Small<HgFileNodeId>, Large<HgFileNodeId>)>,
    Option<(NonRootMPath, Small<FileType>, Large<FileType>)>,
    Option<(
        NonRootMPath,
        Small<FilenodeDiffPayload>,
        Large<FilenodeDiffPayload>,
    )>,
) {
    use FilenodeDiffPayload::*;
    let mut should_be_equivalent = None;
    let mut different_filetypes = None;
    let mut different_actions = None;

    match (small_payload.0, large_payload.0) {
        (Added(small_file_type, small_file_node), Added(large_file_type, large_file_node))
        | (
            ChangedTo(small_file_type, small_file_node),
            ChangedTo(large_file_type, large_file_node),
        ) => {
            if small_file_node != large_file_node {
                should_be_equivalent = Some((
                    mpath.clone(),
                    Small(small_file_node.clone()),
                    Large(large_file_node.clone()),
                ));
            }

            if small_file_type != large_file_type {
                different_filetypes = Some((
                    mpath.clone(),
                    Small(small_file_type.clone()),
                    Large(large_file_type.clone()),
                ));
            }
        }
        (Removed, Removed) => {} // both are `Removed` so there's nothing to compare
        (small_action, large_action) => {
            different_actions = Some((
                mpath.clone(),
                Small(small_action.clone()),
                Large(large_action.clone()),
            ));
        }
    }

    (should_be_equivalent, different_filetypes, different_actions)
}

/// Given full manifest diffs, check that they represent
/// equivalent sets of changes in two repos
async fn validate_full_manifest_diffs_equivalence<'a>(
    ctx: &'a CoreContext,
    validation_helper: &'a ValidationHelper,
    large_cs_id: &'a Large<ChangesetId>,
    small_cs_id: &'a Small<ChangesetId>,
    large_repo_full_manifest_diff: Large<FullManifestDiff>,
    small_repo_full_manifest_diff: Small<FullManifestDiff>,
    small_to_large_mover: Arc<dyn Mover>,
    large_to_small_mover: Arc<dyn Mover>,
) -> Result<(), Error> {
    let moved_large_repo_full_manifest_diff = validation_helper
        .move_full_manifest_diff_large_to_small(
            large_repo_full_manifest_diff,
            large_to_small_mover.as_ref(),
        )?;

    // Let's remove all `FilenodeDiff` structs, which are strictly equivalent
    // Note that `in_small_but_not_in_large` and `in_large_but_not_in_small`
    // do *not* represent different `NonRootMPath`s, but `FilenodeDiff` structs. That is, it
    // is possible that there are two different `FilenodeDiff` structs in the
    // large and small repos, but they correspond to the same `NonRootMPath` and may
    // even represent the exact same `ContentId`!
    let in_small_but_not_in_large: HashMap<Small<&NonRootMPath>, Small<&FilenodeDiffPayload>> =
        small_repo_full_manifest_diff
            .0
            .difference(&moved_large_repo_full_manifest_diff.0)
            .map(|filenode_diff| {
                let (mpath, payload) = filenode_diff.as_tuple();
                (Small(mpath), Small(payload))
            })
            .collect();

    // Note that this hashmap maps small paths to large payloads. This is
    // intentional and represents the idea that paths are already moved
    let mut in_large_but_not_in_small: HashMap<Small<&NonRootMPath>, Large<&FilenodeDiffPayload>> =
        moved_large_repo_full_manifest_diff
            .0
            .difference(&small_repo_full_manifest_diff.0)
            .map(|filenode_diff| {
                let (mpath, payload) = filenode_diff.as_tuple();
                (Small(mpath), Large(payload))
            })
            .collect();

    // This is set of *small* repo changes, which do not have an equivalent
    // in the large repo
    let mut missing_in_large_repo: Vec<(Small<NonRootMPath>, Small<&FilenodeDiffPayload>)> = vec![];

    let mut should_be_equivalent: Vec<(NonRootMPath, Small<HgFileNodeId>, Large<HgFileNodeId>)> =
        vec![];
    let mut different_filetypes: Vec<(NonRootMPath, Small<FileType>, Large<FileType>)> = vec![];
    let mut different_actions: Vec<(
        NonRootMPath,
        Small<FilenodeDiffPayload>,
        Large<FilenodeDiffPayload>,
    )> = vec![];

    for (small_mpath, small_payload) in in_small_but_not_in_large {
        if small_to_large_mover.move_path(small_mpath.0)?.is_none() {
            // It is expected to be missing in the large repo
            continue;
        };

        match in_large_but_not_in_small.remove(&small_mpath) {
            None => {
                // `small_mpath` is present in the small repo, `small_to_large_mover`
                // does not rewrite it into `None`. Seems like a problem!
                let small_mpath: Small<NonRootMPath> = small_mpath.cloned();
                missing_in_large_repo.push((small_mpath, small_payload));
            }
            Some(large_payload) => {
                let (
                    maybe_should_be_equivalent,
                    maybe_different_filetypes,
                    maybe_different_actions,
                ) = compare_diffs_at_mpath(small_mpath.0, small_payload, large_payload);

                should_be_equivalent.extend(maybe_should_be_equivalent);
                different_filetypes.extend(maybe_different_filetypes);
                different_actions.extend(maybe_different_actions);
            }
        };
    }

    // This is a set of *large* repo changes, which do not have an equivalent
    // in the small repo. We need large paths to filter noop ones from these.
    let missing_in_small_repo: Vec<(Large<NonRootMPath>, Large<&FilenodeDiffPayload>)> =
        in_large_but_not_in_small
            .into_iter()
            .map(
                |(small_mpath, payload): (Small<&NonRootMPath>, Large<&FilenodeDiffPayload>)| {
                    // `in_large_but_not_in_small` was initially populated by small
                    // paths, so let's convert
                    let large_mpath =
                        small_to_large_mover
                            .move_path(small_mpath.0)?
                            .ok_or_else(|| {
                                format_err!(
                                    "{:?} unexpectedly produces None when moved small-to-large",
                                    small_mpath.0
                                )
                            })?;

                    Result::<_, Error>::Ok((Large(large_mpath), payload))
                },
            )
            .collect::<Result<Vec<_>, Error>>()?;

    let missing_in_small_repo: Vec<Large<NonRootMPath>> = validation_helper
        .filter_out_large_noop_filenode_id_changes(ctx, large_cs_id, missing_in_small_repo)
        .await?;

    // Note: we populated `missing_in_small_repo` with large repo paths,
    // and `missing_in_large_repo` with small repo paths. Let's unify this
    // to small repo paths, so that we always report the same kind of path.
    let missing_in_large_repo: Vec<Large<NonRootMPath>> = validation_helper
        .filter_out_small_noop_filenode_id_changes(ctx, small_cs_id, missing_in_large_repo)
        .await?
        .into_iter()
        .map(|small_mpath: Small<NonRootMPath>| {
            let large_mpath = small_to_large_mover
                .move_path(&small_mpath.0)?
                .ok_or_else(|| {
                    format_err!(
                        "{small_mpath:?} surprisingly produces None when moved small-to-large"
                    )
                })?;

            Ok(Large(large_mpath))
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let small_name = validation_helper.small_repo.0.repo_identity().name();
    let large_name = validation_helper.large_repo.0.repo_identity().name();

    report_missing(
        ctx,
        missing_in_large_repo,
        large_cs_id,
        small_name,
        large_name,
    )?;
    report_missing(
        ctx,
        missing_in_small_repo,
        large_cs_id,
        large_name,
        small_name,
    )?;

    // In order to call fns, provided by cross_repo_sync/src/validation,
    // we are switching perspective from Large/Small to Source/Taret
    let source_cs_id = Source(large_cs_id.0);
    let source_repo = Source::ref_cast(&validation_helper.large_repo.0);
    let target_repo = Target::ref_cast(&validation_helper.small_repo.0);
    let source_name = Source(large_name);
    let target_name = Target(small_name);

    let different_filetypes = small_large_to_source_target(different_filetypes);
    report_different(
        ctx,
        different_filetypes,
        &source_cs_id,
        "filetype",
        source_name,
        target_name,
    )?;
    let different_actions = small_large_to_source_target(different_actions);
    report_different(
        ctx,
        different_actions,
        &source_cs_id,
        "action",
        source_name,
        target_name,
    )?;

    let should_be_equivalent = small_large_to_source_target(should_be_equivalent);
    verify_filenodes_have_same_contents(
        ctx,
        target_repo,
        source_repo,
        &source_cs_id,
        should_be_equivalent,
    )
    .await?;

    Ok(())
}

/// Rewrap a vector of things, assuming that `Small` is a `Target`,
/// and `Large` is a `Source`
fn small_large_to_source_target<T, P>(
    v: Vec<(T, Small<P>, Large<P>)>,
) -> Vec<(T, Source<P>, Target<P>)> {
    v.into_iter()
        .map(|(t, small, large)| (t, Source(large.0), Target(small.0)))
        .collect()
}

/// Report file changes, present in acommit in one repo,
/// but missing in a commit in another repo
fn report_missing(
    _ctx: &CoreContext,
    missing_things: Vec<Large<NonRootMPath>>,
    large_cs_id: &Large<ChangesetId>,
    where_present: &str,
    where_missing: &str,
) -> Result<(), Error> {
    if !missing_things.is_empty() {
        for missing_thing in missing_things.iter().take(10) {
            debug!(
                "A change to {:?} is present in {}, but missing in {} (large repo cs {})",
                missing_thing, where_present, where_missing, large_cs_id,
            );
        }

        return Err(format_err!(
            "Found {} changes missing in {}, but present in {} (large repo cs {})",
            missing_things.len(),
            where_missing,
            where_present,
            large_cs_id,
        ));
    }

    Ok(())
}

async fn validate_in_a_single_repo(
    ctx: &CoreContext,
    validation_helper: ValidationHelper,
    large_repo: Large<Repo>,
    large_cs_id: Large<ChangesetId>,
    small_cs_id: Small<ChangesetId>,
    large_repo_full_manifest_diff: Large<FullManifestDiff>,
    small_repo_full_manifest_diff: Small<FullManifestDiff>,
    small_to_large_mover: Arc<dyn Mover>,
    large_to_small_mover: Arc<dyn Mover>,
    mapping: SqlSyncedCommitMapping,
) -> Result<(), Error> {
    validate_full_manifest_diffs_equivalence(
        ctx,
        &validation_helper,
        &large_cs_id,
        &small_cs_id,
        large_repo_full_manifest_diff,
        small_repo_full_manifest_diff,
        small_to_large_mover,
        large_to_small_mover,
    )
    .await?;

    validate_topological_order(
        ctx,
        &large_repo,
        large_cs_id,
        &validation_helper.small_repo,
        small_cs_id,
        Arc::new(mapping),
        Arc::new(validation_helper.live_commit_sync_config.clone()),
    )
    .await
}

pub async fn validate_entry(
    ctx: &CoreContext,
    prepared_entry: EntryPreparedForValidation,
    validation_helpers: &ValidationHelpers,
) -> Result<(), Error> {
    let EntryPreparedForValidation {
        entry_id,
        cs_id: large_cs_id,
        large_repo_full_manifest_diff,
        small_repo_full_manifest_diffs,
        queue_size,
        preparation_duration,
    } = prepared_entry;

    let small_repo_validation_futs = small_repo_full_manifest_diffs.into_iter().map(
        |(repo_id, (small_cs_id, small_repo_full_manifest_diff, version_name))| {
            cloned!(large_repo_full_manifest_diff);

            let entry_id = &entry_id;
            let large_repo = &validation_helpers.large_repo;
            let large_cs_id = &large_cs_id;

            async move {
                let validation_helper = validation_helpers
                    .helpers
                    .get(&repo_id)
                    .ok_or_else(|| format_err!("small repo {repo_id} not found"))?
                    .clone();
                let scuba_sample = validation_helper.scuba_sample.clone();
                let mapping = &validation_helpers.mapping;
                let (small_to_large_mover, large_to_small_mover) = validation_helpers
                    .create_movers(&repo_id, &version_name)
                    .await?;

                let (stats, validation_result): (_, Result<Result<(), _>, tokio::task::JoinError>) =
                    mononoke::spawn_task({
                        cloned!(ctx, small_cs_id, large_cs_id, large_repo, mapping);
                        async move {
                            validate_in_a_single_repo(
                                &ctx,
                                validation_helper,
                                large_repo,
                                large_cs_id,
                                small_cs_id,
                                large_repo_full_manifest_diff,
                                small_repo_full_manifest_diff,
                                small_to_large_mover,
                                large_to_small_mover,
                                mapping,
                            )
                            .await
                        }
                    })
                    .timed()
                    .await;

                // `validation_result` is a `Result`of `spawn`, the `Ok`
                // of which contains the `Result` of `validate_in_a_single_repo`
                let validation_result = validation_result?;
                let validation_duration = stats.completion_time;
                let maybe_error_str = match validation_result.as_ref() {
                    Ok(()) => None,
                    Err(e) => {
                        error!("Error while verifying against {}: {:?}", version_name, e);
                        Some(format!("{e}"))
                    }
                };
                log_validation_result_to_scuba(
                    scuba_sample,
                    entry_id.bookmarks_update_log_entry_id,
                    large_cs_id,
                    &small_cs_id,
                    maybe_error_str,
                    queue_size,
                    preparation_duration,
                    validation_duration,
                );

                validation_result
            }
        },
    );
    try_join_all(small_repo_validation_futs).await?;
    info!("Validated entry: {:?}", entry_id);
    Ok(())
}

async fn fetch_root_mf_id(
    ctx: &CoreContext,
    repo: &(impl RepoDerivedDataRef + RepoBlobstoreRef + Send + Sync),
    cs_id: ChangesetId,
) -> Result<HgManifestId, Error> {
    let hg_cs_id = repo.derive_hg_changeset(ctx, cs_id).await?;
    let changeset = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
    Ok(changeset.manifestid())
}

async fn list_all_filenode_ids(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + RepoIdentityRef),
    mf_id: HgManifestId,
) -> Result<PathToFileNodeIdMapping, Error> {
    let repoid = repo.repo_identity().id();
    info!("fetching filenode ids for {:?} in {}", mf_id, repoid,);
    let res = list_all_hg_entries(
        ctx,
        repo.repo_blobstore().clone(),
        repo.repo_identity().name(),
        mf_id,
    )
    .try_filter_map(move |(path, entry)| {
        let res = match entry {
            Entry::Leaf(leaf_payload) => {
                Option::<NonRootMPath>::from(path).map(|path| (path, leaf_payload))
            }
            Entry::Tree(_) => None,
        };
        future::ready(Ok(res))
    })
    .try_collect::<HashMap<_, _>>()
    .await?;

    debug!("fetched {} filenode ids for {}", res.len(), repoid,);
    Ok(res)
}

async fn verify_filenodes_have_same_contents<
    // item is a tuple: (NonRootMPath, large filenode id, small filenode id)
    I: IntoIterator<Item = (NonRootMPath, Source<HgFileNodeId>, Target<HgFileNodeId>)>,
>(
    ctx: &CoreContext,
    target_repo: &Target<Repo>,
    source_repo: &Source<Repo>,
    source_hash: &Source<ChangesetId>,
    should_be_equivalent: I,
) -> Result<(), Error> {
    let fetched_content_ids = stream::iter(should_be_equivalent)
        .map({
            move |(path, source_filenode_id, target_filenode_id)| async move {
                debug!(
                    "checking content for different filenodes: source {} vs target {}",
                    source_filenode_id, target_filenode_id,
                );
                let f1 = async move {
                    source_filenode_id
                        .0
                        .load(ctx, source_repo.0.repo_blobstore())
                        .await
                }
                .map_ok(|e| Source(e.content_id()));
                let f2 = async move {
                    target_filenode_id
                        .0
                        .load(ctx, target_repo.0.repo_blobstore())
                        .await
                }
                .map_ok(|e| Target(e.content_id()));

                let (c1, c2) = future::try_join(f1, f2).await?;
                Result::<_, Error>::Ok((path, c1, c2))
            }
        })
        .buffered(1000)
        .try_collect::<Vec<_>>()
        .await?;

    let different_contents: Vec<_> = fetched_content_ids
        .into_iter()
        .filter(|(_mpath, c1, c2)| c1.0 != c2.0)
        .collect();

    report_different(
        ctx,
        different_contents,
        source_hash,
        "contents",
        Source(source_repo.0.repo_identity().name()),
        Target(target_repo.0.repo_identity().name()),
    )
}

#[cfg(test)]
mod tests {
    use blobstore::KeyedBlobstore;
    use cross_repo_sync::test_utils::TestRepo;
    use cross_repo_sync::test_utils::init_small_large_repo;
    use cross_repo_sync::test_utils::xrepo_mapping_version_with_small_repo;
    use cross_repo_sync::update_mapping_with_version;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use maplit::hashmap;
    use memblob::Memblob;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mercurial_types::HgAugmentedManifestId;
    use mercurial_types::NULL_HASH;
    use mononoke_macros::mononoke;
    use mononoke_types::MPath;
    use repo_derived_data::RepoDerivedDataRef;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::BasicTestRepo;
    use tests_utils::CommitIdentifier;
    use tests_utils::CreateCommitContext;
    use tests_utils::hg_manifest_reads::HgManifestReadBlocker;

    use super::*;

    async fn add_commits_to_repo(
        ctx: &CoreContext,
        spec: Vec<HashMap<&str, &str>>,
        repo: &TestRepo,
    ) -> Result<Vec<ChangesetId>, Error> {
        let mut parent: CommitIdentifier = "master".into();
        let mut commits: Vec<ChangesetId> = vec![];
        for file_changes in spec {
            let commit = CreateCommitContext::new(ctx, repo, vec![parent])
                .add_files(file_changes)
                .commit()
                .await?;
            commits.push(commit.clone());
            parent = commit.into();
        }

        Ok(commits)
    }

    /// Initialize small and large repos, create commits according
    /// to provided specifications  and update the SyncedCommitMapping.
    /// `large_repo_commits_spec`, `small_repo_commits_spec` are specs for new commits.
    /// Each spec will trigger a linear chain of commits in its repo with
    /// appropriate file changes.
    /// `commit_index_mapping` is a mapping specification, which says:
    /// "for each pair of indices `i=>j` in this hashmap, please take
    /// i-th produced commit in the large repo and map to j'th produced
    /// commit in the small repo.
    async fn test_topological_order_validation(
        fb: FacebookInit,
        large_repo_commits_spec: Vec<HashMap<&str, &str>>,
        small_repo_commits_spec: Vec<HashMap<&str, &str>>,
        commit_index_mapping: HashMap<usize, usize>,
        large_index_to_test: usize,
        small_index_to_test: usize,
    ) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (syncers, _, _, _) = init_small_large_repo(&ctx).await?;
        let small_to_large_commit_syncer = syncers.small_to_large;
        let small_repo = small_to_large_commit_syncer.get_small_repo();
        let large_repo = small_to_large_commit_syncer.get_large_repo();

        let large_commits = add_commits_to_repo(&ctx, large_repo_commits_spec, large_repo).await?;
        let small_commits = add_commits_to_repo(&ctx, small_repo_commits_spec, small_repo).await?;
        let commit_mapping: HashMap<ChangesetId, ChangesetId> = commit_index_mapping
            .into_iter()
            .map(|(large_index, small_index)| {
                (
                    small_commits[small_index].clone(),
                    large_commits[large_index].clone(),
                )
            })
            .collect();

        update_mapping_with_version(
            &ctx,
            commit_mapping,
            &small_to_large_commit_syncer,
            &xrepo_mapping_version_with_small_repo(),
        )
        .await?;
        let large_repo = Large(large_repo.clone());
        let small_repo = Small(small_repo.clone());

        validate_topological_order(
            &ctx,
            &large_repo,
            Large(large_commits[large_index_to_test].clone()),
            &small_repo,
            Small(small_commits[small_index_to_test].clone()),
            small_to_large_commit_syncer.get_mapping().clone(),
            small_to_large_commit_syncer.live_commit_sync_config,
        )
        .await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_topological_order_validation_ok(fb: FacebookInit) -> Result<(), Error> {
        // Large repo  Mapping  Small repo
        //    2    -------------    1
        //    |                     |
        //    1                     |
        //    |                     |
        //    0    -------------    0
        //   ..                     ..
        let validation_result = test_topological_order_validation(
            fb,
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile1" => "newcontent"},
                hashmap! {"newfile2" => "newcontent"},
            ],
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile2" => "newcontent"},
            ],
            hashmap! {0 => 0, 2 => 1},
            2,
            1,
        )
        .await;
        assert!(validation_result.is_ok());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_topological_order_validation_bad_direct_parents(
        fb: FacebookInit,
    ) -> Result<(), Error> {
        // Large repo  Mapping  Small repo
        //    1      --\   /--      1
        //    |         \ /         |
        //    |          X          |
        //    |         / \         |
        //    0      --/   \--      0
        //   ..                     ..
        let validation_result = test_topological_order_validation(
            fb,
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile1" => "newcontent"},
            ],
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile1" => "newcontent"},
            ],
            hashmap! {0 => 1, 1 => 0},
            0,
            1,
        )
        .await;
        assert!(validation_result.is_err());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_topological_order_validation_bad_ancestors(
        fb: FacebookInit,
    ) -> Result<(), Error> {
        // Large repo  Mapping  Small repo
        //    2      --\   /--      1
        //    |         \ /         |
        //    1          X          |
        //    |         / \         |
        //    0      --/   \--      0
        //   ..                     ..
        let validation_result = test_topological_order_validation(
            fb,
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile1" => "newcontent"},
                hashmap! {"newfile2" => "newcontent"},
            ],
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile2" => "newcontent"},
            ],
            hashmap! {0 => 1, 2 => 0},
            0,
            1,
        )
        .await;
        assert!(validation_result.is_err());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_topological_order_validation_bad_unremapped_ancestors(
        fb: FacebookInit,
    ) -> Result<(), Error> {
        // Large repo  Mapping  Small repo
        //    2      ---------      1
        //    |                     |
        //    1                     |
        //    |                     |
        //    0                     0
        //   ..                     ..
        let validation_result = test_topological_order_validation(
            fb,
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile1" => "newcontent"},
                hashmap! {"newfile2" => "newcontent"},
            ],
            vec![
                hashmap! {"newfile0" => "newcontent"},
                hashmap! {"newfile2" => "newcontent"},
            ],
            hashmap! {2 => 1},
            2,
            1,
        )
        .await;
        assert!(validation_result.is_err());
        Ok(())
    }

    const READ_FROM_AUGMENTED: &str = "scm/mononoke:hgmanifest_read_from_augmented";

    fn read_from_augmented(on: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(hashmap! {
            READ_FROM_AUGMENTED.to_string() => KnobVal::Bool(on),
        })
    }

    fn no_augmented_with_hg_changeset() -> JustKnobsInMemory {
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
        })
    }

    struct Fixture {
        repo: BasicTestRepo,
        blocker: Arc<HgManifestReadBlocker>,
        child: ChangesetId,
        root_mf_id: HgManifestId,
        child_mf_id: HgManifestId,
    }

    /// A repo whose Hg manifest reads fail once the blocker is armed, with a
    /// root commit holding `a` and `dir/b`, and a child that removes `a`,
    /// changes `dir/b` and adds `c`. Only the root commit has an augmented
    /// manifest.
    async fn fixture(ctx: &CoreContext) -> Result<Fixture, Error> {
        let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
        let repo: BasicTestRepo = TestRepoFactory::new(ctx.fb)?
            .with_blobstore(blobstore)
            .build()
            .await?;
        let root = CreateCommitContext::new_root(ctx, &repo)
            .add_file("a", "a")
            .add_file("dir/b", "b")
            .commit()
            .await?;
        let child = CreateCommitContext::new(ctx, &repo, vec![root])
            .delete_file("a")
            .add_file("dir/b", "b2")
            .add_file("c", "c")
            .commit()
            .await?;
        // Derive the hg changesets without their augmented manifests:
        // derivation would also derive augmented manifests, so store the Hg
        // changesets as an upload does.
        fixtures::store_hg_changesets_as_uploaded(ctx, &repo, vec![root, child]).await?;
        let (root_mf_id, child_mf_id) = with_just_knobs_async(
            no_augmented_with_hg_changeset(),
            async {
                anyhow::Ok((
                    fetch_root_mf_id(ctx, &repo, root).await?,
                    fetch_root_mf_id(ctx, &repo, child).await?,
                ))
            }
            .boxed(),
        )
        .await?;
        derive_augmented(ctx, &repo, root).await?;
        Ok(Fixture {
            repo,
            blocker,
            child,
            root_mf_id,
            child_mf_id,
        })
    }

    async fn derive_augmented(
        ctx: &CoreContext,
        repo: &BasicTestRepo,
        cs_id: ChangesetId,
    ) -> Result<(), Error> {
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
        Ok(())
    }

    type ValidatorReads = (
        PathToFileNodeIdMapping,
        FullManifestDiff,
        Option<Entry<HgManifestId, (FileType, HgFileNodeId)>>,
        Option<Entry<HgManifestId, (FileType, HgFileNodeId)>>,
    );

    /// What the validator reads from a pair of root manifests: all files of
    /// the old one, the filenode diff between them, and the old one's entries
    /// for a directory and a file.
    async fn validator_reads(
        ctx: &CoreContext,
        repo: &BasicTestRepo,
        old: HgManifestId,
        new: HgManifestId,
    ) -> Result<ValidatorReads, Error> {
        let files = list_all_filenode_ids(ctx, repo, old).await?;
        let blobstore = repo.repo_blobstore().clone();
        let name = repo.repo_identity().name();
        let diff = hg_manifest_diff(ctx, blobstore.clone(), name, old, new)
            .try_filter_map(|diff| future::ok(FilenodeDiff::from_diff(diff)))
            .try_collect()
            .await?;
        let dir = find_hg_entry(ctx, blobstore.clone(), name, old, MPath::new("dir")?).await?;
        let file = find_hg_entry(ctx, blobstore, name, old, MPath::new("dir/b")?).await?;
        Ok((files, diff, dir, file))
    }

    #[mononoke::fbinit_test]
    async fn test_manifest_reads_from_augmented(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let f = fixture(&ctx).await?;
        derive_augmented(&ctx, &f.repo, f.child).await?;

        // Given: what the validator reads from Hg manifests.
        let from_hg = with_just_knobs_async(
            read_from_augmented(false),
            validator_reads(&ctx, &f.repo, f.root_mf_id, f.child_mf_id).boxed(),
        )
        .await?;
        let (files, diff, dir, file) = &from_hg;
        assert_eq!(files.len(), 2, "the root commit has two files");
        assert_eq!(diff.len(), 3, "the child removes, changes and adds a file");
        assert!(matches!(dir, Some(Entry::Tree(_))));
        assert!(matches!(file, Some(Entry::Leaf((FileType::Regular, _)))));

        // When: Hg manifests can no longer be read.
        f.blocker.arm();

        // Then: augmented manifests give the same results.
        let from_augmented = with_just_knobs_async(
            read_from_augmented(true),
            validator_reads(&ctx, &f.repo, f.root_mf_id, f.child_mf_id).boxed(),
        )
        .await?;
        assert_eq!(from_augmented, from_hg);
        assert_eq!(
            f.blocker.reads(),
            0,
            "no Hg manifest is read with the knob on"
        );

        // And: with the knob off, the Hg manifests are still read.
        let knob_off = with_just_knobs_async(
            read_from_augmented(false),
            validator_reads(&ctx, &f.repo, f.root_mf_id, f.child_mf_id).boxed(),
        )
        .await;
        assert!(knob_off.is_err(), "the knob off reads blocked Hg manifests");
        assert!(f.blocker.reads() > 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_missing_augmented_manifest(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let f = fixture(&ctx).await?;
        let child_key = HgAugmentedManifestId::from(f.child_mf_id).blobstore_key();
        assert!(
            f.repo
                .repo_blobstore()
                .get(&ctx, &child_key)
                .await?
                .is_none(),
            "the child has no augmented manifest"
        );
        f.blocker.arm();

        let err = with_just_knobs_async(
            read_from_augmented(true),
            validator_reads(&ctx, &f.repo, f.child_mf_id, f.root_mf_id).boxed(),
        )
        .await
        .expect_err("a missing augmented manifest is an error");
        assert!(
            format!("{err:#}").contains(&child_key),
            "the error names the missing augmented manifest: {err:#}"
        );
        assert_eq!(
            f.blocker.reads(),
            0,
            "there is no fallback to the Hg manifest"
        );
        Ok(())
    }

    /// What the validator reads when one side of a diff has the null
    /// manifest, which is empty: first as the old side, then as the new side.
    async fn null_manifest_reads(
        ctx: &CoreContext,
        repo: &BasicTestRepo,
        mf_id: HgManifestId,
    ) -> Result<(ValidatorReads, ValidatorReads), Error> {
        let null = HgManifestId::new(NULL_HASH);
        Ok((
            validator_reads(ctx, repo, null, mf_id).await?,
            validator_reads(ctx, repo, mf_id, null).await?,
        ))
    }

    #[mononoke::fbinit_test]
    async fn test_null_manifest_reads_from_augmented(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let f = fixture(&ctx).await?;

        // Given: what the validator reads from Hg manifests, which treat the
        // null manifest as empty.
        let from_hg = with_just_knobs_async(
            read_from_augmented(false),
            null_manifest_reads(&ctx, &f.repo, f.root_mf_id).boxed(),
        )
        .await?;
        let ((files, added, dir, file), (_, removed, _, _)) = &from_hg;
        assert!(files.is_empty(), "the null manifest has no files");
        assert_eq!(added.len(), 2, "both files of the root commit are added");
        assert_eq!(
            removed.len(),
            2,
            "both files of the root commit are removed"
        );
        assert_eq!((dir, file), (&None, &None));

        // When: Hg manifests can no longer be read.
        f.blocker.arm();

        // Then: augmented manifests give the same results.
        let from_augmented = with_just_knobs_async(
            read_from_augmented(true),
            null_manifest_reads(&ctx, &f.repo, f.root_mf_id).boxed(),
        )
        .await?;
        assert_eq!(from_augmented, from_hg);
        assert_eq!(
            f.blocker.reads(),
            0,
            "no Hg manifest is read with the knob on"
        );
        Ok(())
    }
}
