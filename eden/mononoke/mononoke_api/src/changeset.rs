/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Display;
use std::future::Future;
use std::iter;
use std::ops::Bound;
use std::sync::Arc;

use acl_regions::AclRegionsRef;
use anyhow::anyhow;
use basename_suffix_skeleton_manifest_v3::RootBssmV3DirectoryId;
use blobstore::Loadable;
use bonsai_git_mapping::BonsaiGitMappingRef;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingRef;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bonsai_p4_mapping::BonsaiP4MappingRef;
use bonsai_svnrev_mapping::BonsaiSvnrevMappingRef;
use bookmarks::BookmarkKey;
use bytes::Bytes;
use changeset_info::ChangesetInfo;
use chrono::DateTime;
use chrono::FixedOffset;
use cloned::cloned;
use commit_graph::AncestorsStreamBuilder;
use commit_graph::CommitGraphArc;
use commit_graph::CommitGraphRef;
use commit_graph::LinearAncestorsStreamBuilder;
use commit_rate_limit::CommitRateLimitCheckResult;
use commit_rate_limit::check_all_commit_rate_limits;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use deleted_manifest::DeletedManifestOps;
use deleted_manifest::RootDeletedManifestIdCommon;
use deleted_manifest::RootDeletedManifestV2Id;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivableType;
use directory_branch_cluster_manifest::RootDirectoryBranchClusterManifestId;
use ephemeral_blobstore::RepoEphemeralStoreRef;
use fsnodes::RootFsnodeId;
use futures::future::try_join;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_ext::FbStreamExt;
use futures_lazy_shared::LazyShared;
use futures_watchdog::WatchdogExt;
use git_types::MappedGitCommitId;
use hooks::CrossRepoPushSource;
use hooks::HookExecutionPurpose;
use hooks::HookOutcome;
use hooks::LogOnlyRejections;
use hooks::PushAuthoredBy;
use itertools::Itertools;
use manifest::Diff as ManifestDiff;
use manifest::Entry as ManifestEntry;
use manifest::ManifestOps;
use manifest::ManifestOrderedOps;
use manifest::PathOrPrefix;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_types::Globalrev;
use metaconfig_types::RepoConfigRef;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ContentManifestId;
use mononoke_types::FileChange;
pub use mononoke_types::Generation;
use mononoke_types::NonRootMPath;
use mononoke_types::P4ChangelistId;
use mononoke_types::SkeletonManifestId;
use mononoke_types::SubtreeChange;
use mononoke_types::Svnrev;
use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifest;
use mononoke_types::path::MPath;
use mononoke_types::skeleton_manifest_v2::SkeletonManifestV2;
use mutable_renames::MutableRenamesArc;
use permission_checker::MononokeIdentitySet;
use phases::PhasesRef;
use repo_blobstore::RepoBlobstoreArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use repo_permission_checker::RepoPermissionCheckerRef;
use restricted_paths::DenyingRestriction;
use restricted_paths::RestrictedPathsArc;
use restricted_paths::check_path_restriction_infos;
use skeleton_manifest::RootSkeletonManifestId;
use skeleton_manifest_v2::RootSkeletonManifestV2Id;
use smallvec::SmallVec;
use sorted_vector_map::SortedVectorMap;
use tracing::warn;
use unodes::RootUnodeManifestId;
use vec1::Vec1;
use xdiff::CopyInfo;

use crate::MononokeRepo;
use crate::changeset_path::ChangesetPathContentContext;
use crate::changeset_path::ChangesetPathContext;
use crate::changeset_path::ChangesetPathHistoryContext;
use crate::changeset_path::ChangesetPathRestrictionContext;
use crate::changeset_path_diff::ChangesetPathDiffContext;
use crate::errors::MononokeError;
use crate::repo::RepoContext;
use crate::repo::RepoWithBubble;
use crate::restricted_paths::OmissionCollector;
use crate::restricted_paths::PathAccessInfo;
use crate::restricted_paths::RestrictedPathsPolicy;
use crate::specifiers::ChangesetId;
use crate::specifiers::GitSha1;
use crate::specifiers::HgChangesetId;

mod find_files;
mod restricted_paths_changes;

const ENABLE_BONSAI_ONLY_LFS_DIFFS: &str = "scm/mononoke:changeset_diff_enable_bonsai_only_lfs";

/// Algorithm version for changeset content fingerprinting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerprintVersion {
    /// V1: root FsnodeId blake2 hash. Universally available — every production
    /// repo derives Fsnodes — but not the long-term recommendation.
    V1,
    /// V2: root ContentManifestId blake2 hash. Recommended long-term default.
    /// Requires ContentManifests derived data enabled for
    /// the repo; otherwise the request fails with InvalidRequest (no
    /// auto-fallback, to keep the fingerprint bytes stable for consumers).
    V2,
}

#[derive(Clone, Debug)]
enum PathMutableHistory {
    /// Checking the mutable history datastore shows no changes
    NoChange,
    /// Change of parent and path
    PathAndChangeset(ChangesetId, MPath),
}

impl PathMutableHistory {
    /// Get the mutable parents (if any) of this path
    pub fn get_parent_cs_id(&self) -> Option<ChangesetId> {
        match self {
            Self::NoChange => None,
            Self::PathAndChangeset(cs_id, _) => Some(*cs_id),
        }
    }

    /// Is this path overridden by mutable history?
    fn is_override(&self) -> bool {
        match self {
            Self::NoChange => false,
            Self::PathAndChangeset(_, _) => true,
        }
    }

    /// Extract the copy_from information relating to this entry, if any
    fn get_copy_from(&self) -> Option<(ChangesetId, &MPath)> {
        match self {
            Self::NoChange => None,
            Self::PathAndChangeset(cs_id, path) => Some((*cs_id, path)),
        }
    }
}

pub struct DirectoryBranchCluster {
    pub cluster_primary: MPath,
    pub secondaries: Vec<MPath>,
}

impl From<metaconfig_types::DirectoryBranchClusterFixedCluster> for DirectoryBranchCluster {
    fn from(cluster: metaconfig_types::DirectoryBranchClusterFixedCluster) -> Self {
        Self {
            cluster_primary: MPath::from(cluster.cluster_primary),
            secondaries: cluster.secondaries.into_iter().map(MPath::from).collect(),
        }
    }
}

/// Recursively traverse the directory branch cluster manifest to collect all clusters.
/// A cluster is found when a directory has non-empty secondaries (it is a primary).
//
// Note: We expect the number of clusters to be relatively small (O(1000)),
// and the manifest only contains directories that are part of clusters, not
// all directories in the repo, so traversing the whole manifest should be fine.
async fn collect_manifest_clusters(
    ctx: &CoreContext,
    blobstore: &repo_blobstore::RepoBlobstore,
    manifest: DirectoryBranchClusterManifest,
    current_path: MPath,
) -> Result<Vec<DirectoryBranchCluster>, MononokeError> {
    use mononoke_types::directory_branch_cluster_manifest::DirectoryBranchClusterManifestEntry;

    let mut clusters = Vec::new();

    // Check if this directory is a cluster primary (has secondaries)
    if let Some(secondaries) = manifest.get_secondaries() {
        if !secondaries.is_empty() {
            clusters.push(DirectoryBranchCluster {
                cluster_primary: current_path.clone(),
                secondaries: secondaries.to_vec(),
            });
        }
    }

    // Recursively traverse child directories (skip file entries)
    let mut subentries = manifest.into_subentries(ctx, blobstore);
    while let Some(result) = subentries.next().await {
        let (name, entry) = result.map_err(MononokeError::from)?;
        // Only recurse into directories, skip file entries
        if let DirectoryBranchClusterManifestEntry::Directory(child_manifest) = entry {
            let child_path = current_path.join_element(Some(&name));
            let child_clusters = Box::pin(collect_manifest_clusters(
                ctx,
                blobstore,
                child_manifest,
                child_path,
            ))
            .await?;
            clusters.extend(child_clusters);
        }
    }

    Ok(clusters)
}

/// A context object representing a query to a particular commit in a repo.
#[derive(Clone)]
pub struct ChangesetContext<R> {
    repo_ctx: RepoContext<R>,
    id: ChangesetId,
    bonsai_changeset: LazyShared<Result<BonsaiChangeset, MononokeError>>,
    changeset_info: LazyShared<Result<ChangesetInfo, MononokeError>>,
    root_unode_manifest_id: LazyShared<Result<RootUnodeManifestId, MononokeError>>,
    root_content_manifest_id: LazyShared<Result<ContentManifestId, MononokeError>>,
    root_skeleton_manifest_id: LazyShared<Result<RootSkeletonManifestId, MononokeError>>,
    root_skeleton_manifest_v2_id: LazyShared<Result<RootSkeletonManifestV2Id, MononokeError>>,
    root_deleted_manifest_v2_id: LazyShared<Result<RootDeletedManifestV2Id, MononokeError>>,
    root_bssm_v3_directory_id: LazyShared<Result<RootBssmV3DirectoryId, MononokeError>>,
    root_directory_branch_cluster_manifest_id:
        LazyShared<Result<RootDirectoryBranchClusterManifestId, MononokeError>>,
    /// None if no mutable history, else map from supplied paths to data fetched
    mutable_history: Option<HashMap<MPath, PathMutableHistory>>,
}

#[derive(Default)]
pub struct ChangesetHistoryOptions {
    pub until_timestamp: Option<i64>,
    pub until_committer_timestamp: Option<i64>,
    pub descendants_of: Option<ChangesetId>,
    pub exclude_changeset_and_ancestors: Option<ChangesetId>,
}

#[derive(Default)]
pub struct ChangesetLinearHistoryOptions {
    pub descendants_of: Option<ChangesetId>,
    pub exclude_changeset_and_ancestors: Option<ChangesetId>,
    pub skip: u64,
}

#[derive(Clone)]
pub enum ChangesetFileOrdering {
    Unordered,
    Ordered { after: Option<MPath> },
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangesetDiffItem {
    TREES,
    FILES,
}

impl<R: RepoIdentityRef> fmt::Debug for ChangesetContext<R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "ChangesetContext(repo={:?} id={:?})",
            self.repo_ctx().name(),
            self.id()
        )
    }
}

fn to_vec1<X>(maybe_vec: Option<Vec<X>>) -> Option<Vec1<X>> {
    maybe_vec.and_then(|v| Vec1::try_from_vec(v).ok())
}

/// How a requested path resolved under partial-response semantics.
pub enum PathVisibility<R> {
    /// Caller is entitled: content context for the visible side.
    Present(ChangesetPathContentContext<R>),
    /// Restricted-path denial, carrying the denying restriction.
    Denied(DenyingRestriction),
}

/// Classify one side of a diff pair under partial-response semantics.
///
/// `Strict` propagates any error. `SkipAndCollect` converts only
/// restricted-path denials into `None` (recording them against the denying
/// restriction); repo-auth denials and all other errors still fail. Callers
/// map `None` to omit-or-downgrade per the per-side absent rule.
async fn policy_side<R, F>(
    fut: F,
    policy: &RestrictedPathsPolicy,
) -> Result<Option<ChangesetPathContentContext<R>>, MononokeError>
where
    F: Future<Output = Result<ChangesetPathContentContext<R>, MononokeError>>,
{
    match fut.await {
        Ok(ctx) => Ok(Some(ctx)),
        Err(e) => match policy {
            RestrictedPathsPolicy::SkipAndCollect(collector)
                if let Some(denial) = e.restricted_paths_denial() =>
            {
                collector.record(denial.denying_restriction());
                Ok(None)
            }
            _ => Err(e),
        },
    }
}

/// Map a fallible stream item into an optional, recording restricted-path
/// denials against their denying restriction and dropping those items. All
/// other errors still fail the stream.
fn skip_restricted_denials<S, T>(
    stream: S,
    collector: Arc<OmissionCollector>,
) -> impl Stream<Item = Result<T, MononokeError>>
where
    S: Stream<Item = Result<T, MononokeError>>,
{
    stream.filter_map(move |res| match res {
        Err(e) => {
            if let Some(denial) = e.restricted_paths_denial() {
                collector.record(denial.denying_restriction());
                futures::future::ready(None)
            } else {
                futures::future::ready(Some(Err(e)))
            }
        }
        Ok(item) => futures::future::ready(Some(Ok(item))),
    })
}

impl<R> ChangesetContext<R> {
    /// Construct a new `MononokeChangeset`.  The changeset must exist
    /// in the repo.
    pub(crate) fn new(repo_ctx: RepoContext<R>, id: ChangesetId) -> Self {
        let bonsai_changeset = LazyShared::new_empty();
        let changeset_info = LazyShared::new_empty();
        let root_unode_manifest_id = LazyShared::new_empty();
        let root_content_manifest_id = LazyShared::new_empty();
        let root_skeleton_manifest_id = LazyShared::new_empty();
        let root_skeleton_manifest_v2_id = LazyShared::new_empty();
        let root_deleted_manifest_v2_id = LazyShared::new_empty();
        let root_bssm_v3_directory_id = LazyShared::new_empty();
        let root_directory_branch_cluster_manifest_id = LazyShared::new_empty();
        Self {
            repo_ctx,
            id,
            changeset_info,
            bonsai_changeset,
            root_unode_manifest_id,
            root_content_manifest_id,
            root_skeleton_manifest_id,
            root_skeleton_manifest_v2_id,
            root_deleted_manifest_v2_id,
            root_bssm_v3_directory_id,
            root_directory_branch_cluster_manifest_id,
            mutable_history: None,
        }
    }

    /// The context for this query.
    pub fn ctx(&self) -> &CoreContext {
        self.repo_ctx.ctx()
    }

    /// The `RepoContext` for this query.
    pub fn repo_ctx(&self) -> &RepoContext<R> {
        &self.repo_ctx
    }

    /// The canonical bonsai changeset ID for the changeset.
    pub fn id(&self) -> ChangesetId {
        self.id
    }

    /// Deconstruct the changeset into RepoContext and ChangesetId.
    pub fn into_repo_ctx_and_id(self) -> (RepoContext<R>, ChangesetId) {
        let Self { repo_ctx, id, .. } = self;
        (repo_ctx, id)
    }

    /// The IDs of mutable parents of the changeset, if any.
    ///
    /// The value can be `None` to indicate that we were given a path
    /// to check, but it had no mutable parents of its own.
    ///
    /// Only returns a non-empty set if add_mutable_renames has been called
    pub fn mutable_parents(&self) -> HashSet<Option<ChangesetId>> {
        self.mutable_history
            .as_ref()
            .map_or(HashSet::new(), |info| {
                info.values()
                    .map(PathMutableHistory::get_parent_cs_id)
                    .collect()
            })
    }
}

impl<R: MutableRenamesArc> ChangesetContext<R> {
    /// Adds copy information from mutable renames as an override to replace
    /// the Bonsai copy information
    pub async fn add_mutable_renames(
        &mut self,
        paths: impl Iterator<Item = MPath>,
    ) -> Result<(), MononokeError> {
        let mutable_renames = &self.repo_ctx.mutable_renames();
        let ctx = self.ctx();
        let cs_id = self.id;

        let copy_info = stream::iter(paths.map(move |path| async move {
            let maybe_rename_entry = mutable_renames.get_rename(ctx, cs_id, path.clone()).await?;
            let rename = match maybe_rename_entry {
                Some(entry) => {
                    let cs_id = entry.src_cs_id();
                    let path = entry.src_path().clone();
                    PathMutableHistory::PathAndChangeset(cs_id, path)
                }
                None => PathMutableHistory::NoChange,
            };
            Ok::<_, MononokeError>((path, rename))
        }))
        .buffered(100)
        .try_collect()
        .await?;

        self.mutable_history = Some(copy_info);
        Ok(())
    }
}

impl<R: BonsaiGlobalrevMappingRef> ChangesetContext<R> {
    /// The Globalrev for the changeset.
    pub async fn globalrev(&self) -> Result<Option<Globalrev>, MononokeError> {
        let mapping = self
            .repo_ctx()
            .repo()
            .bonsai_globalrev_mapping()
            .get_globalrev_from_bonsai(self.ctx(), self.id)
            .await?;
        Ok(mapping.into_iter().next())
    }
}

impl<R: BonsaiSvnrevMappingRef> ChangesetContext<R> {
    /// The SVN revision number for the changeset.
    pub async fn svnrev(&self) -> Result<Option<Svnrev>, MononokeError> {
        let mapping = self
            .repo_ctx()
            .repo()
            .bonsai_svnrev_mapping()
            .get_svnrev_from_bonsai(self.ctx(), self.id)
            .await?;
        Ok(mapping)
    }
}

impl<R: BonsaiP4MappingRef> ChangesetContext<R> {
    /// The Perforce changelist this changeset was imported from, for mirror repos.
    pub async fn p4_changelist_id(&self) -> Result<Option<P4ChangelistId>, MononokeError> {
        let mapping = self
            .repo_ctx()
            .repo()
            .bonsai_p4_mapping()
            .get_p4_changelist_id_from_bonsai(self.ctx(), self.id)
            .await?;
        Ok(mapping)
    }
}

impl<R: RepoDerivedDataArc> ChangesetContext<R> {
    /// Derive a derivable data type for this changeset.
    // Desugared async syntax so we can return a future with static lifetime.
    fn derive<Derivable: BonsaiDerivable>(
        &self,
    ) -> impl Future<Output = Result<Derivable, MononokeError>> + Send + 'static + use<Derivable, R>
    {
        let ctx = self.ctx().clone();
        let repo_derived_data = self.repo_ctx.repo().repo_derived_data_arc();
        let id = self.id;
        async move {
            repo_derived_data
                .derive::<Derivable>(&ctx, id, DerivationPriority::LOW)
                .await
                .map_err(MononokeError::from)
        }
    }

    /// Returns a content-based fingerprint for this changeset.
    ///
    /// The fingerprint depends only on the file tree contents (paths, content
    /// ids, file types, sizes), not on metadata (author, date, message).
    /// Two changesets with identical file trees produce the same fingerprint
    /// regardless of how they got there.
    pub async fn content_fingerprint(
        &self,
        version: FingerprintVersion,
    ) -> Result<Vec<u8>, MononokeError> {
        match version {
            FingerprintVersion::V1 => {
                let root_id = self.derive::<RootFsnodeId>().await?;
                Ok(root_id.into_fsnode_id().blake2().as_ref().to_vec())
            }
            FingerprintVersion::V2 => {
                // No auto-fallback to fsnode if ContentManifest is not enrolled:
                // the deterministic-fingerprint guarantee requires V2 bytes to
                // be derivable only from RootContentManifestId. The
                // derive::<RootContentManifestId>() call returns InvalidRequest
                // when the type is not enabled for the repo.
                let root_id = self.derive::<RootContentManifestId>().await?;
                Ok(root_id
                    .into_content_manifest_id()
                    .blake2()
                    .as_ref()
                    .to_vec())
            }
        }
    }

    pub(crate) async fn root_unode_manifest_id(
        &self,
    ) -> Result<RootUnodeManifestId, MononokeError> {
        self.root_unode_manifest_id
            .get_or_init(|| self.derive::<RootUnodeManifestId>())
            .await
    }

    pub(crate) async fn root_bssm_v3_directory_id(
        &self,
    ) -> Result<RootBssmV3DirectoryId, MononokeError> {
        self.root_bssm_v3_directory_id
            .get_or_init(|| self.derive::<RootBssmV3DirectoryId>())
            .await
    }

    pub(crate) async fn root_skeleton_manifest_id(
        &self,
    ) -> Result<RootSkeletonManifestId, MononokeError> {
        self.root_skeleton_manifest_id
            .get_or_init(|| self.derive::<RootSkeletonManifestId>())
            .await
    }

    pub(crate) async fn root_skeleton_manifest_v2_id(
        &self,
    ) -> Result<RootSkeletonManifestV2Id, MononokeError> {
        self.root_skeleton_manifest_v2_id
            .get_or_init(|| self.derive::<RootSkeletonManifestV2Id>())
            .await
    }

    pub(crate) async fn root_deleted_manifest_v2_id(
        &self,
    ) -> Result<RootDeletedManifestV2Id, MononokeError> {
        self.root_deleted_manifest_v2_id
            .get_or_init(|| self.derive::<RootDeletedManifestV2Id>())
            .await
    }

    pub(crate) async fn root_directory_branch_cluster_manifest_id(
        &self,
    ) -> Result<RootDirectoryBranchClusterManifestId, MononokeError> {
        self.root_directory_branch_cluster_manifest_id
            .get_or_init(|| self.derive::<RootDirectoryBranchClusterManifestId>())
            .await
    }
}

impl<R: RepoDerivedDataArc + RepoIdentityRef> ChangesetContext<R> {
    pub(crate) async fn root_content_manifest_id(
        &self,
    ) -> Result<ContentManifestId, MononokeError> {
        self.root_content_manifest_id
            .get_or_init(|| {
                let fut = self.derive::<RootContentManifestId>();
                async move { Ok(fut.await?.into_content_manifest_id()) }
            })
            .await
    }

    /// Derive the root content manifest, memoizing it on this context and its clones.
    pub async fn prewarm_root_content_manifest(&self) -> Result<(), MononokeError> {
        self.root_content_manifest_id().await?;
        Ok(())
    }
}

impl<R: RepoBlobstoreArc> ChangesetContext<R> {
    /// Get the `BonsaiChangeset` information for this changeset.
    pub async fn bonsai_changeset(&self) -> Result<BonsaiChangeset, MononokeError> {
        self.bonsai_changeset
            .get_or_init(|| {
                let ctx = self.ctx().clone();
                let blobstore = self.repo_ctx.repo().repo_blobstore_arc();
                let id = self.id;
                async move { id.load(&ctx, &blobstore).await.map_err(MononokeError::from) }
            })
            .await
    }
}

impl<R: CommitGraphRef + Clone> ChangesetContext<R> {
    /// The generation number of the given changeset
    pub async fn generation(&self) -> Result<Generation, MononokeError> {
        self.repo_ctx
            .commit_graph()
            .changeset_generation(self.ctx(), self.id)
            .await
            .map_err(|_| {
                MononokeError::NotAvailable(format!("Generation number missing for {:?}", self.id))
            })
    }

    /// The linear depth of the given changeset
    pub async fn linear_depth(&self) -> Result<u64, MononokeError> {
        self.repo_ctx
            .commit_graph()
            .changeset_linear_depth(self.ctx(), self.id)
            .await
            .map_err(|_| {
                MononokeError::NotAvailable(format!("Linear depth missing for {:?}", self.id))
            })
    }

    /// Returns `true` if this commit is an ancestor of `other_commit`.  A commit is considered its
    /// own ancestor for the purpose of this call.
    pub async fn is_ancestor_of(&self, other_commit: ChangesetId) -> Result<bool, MononokeError> {
        Ok(self
            .repo_ctx()
            .repo()
            .commit_graph()
            .is_ancestor(self.ctx(), self.id, other_commit)
            .await?)
    }

    /// Returns the lowest common ancestor of two commits.
    ///
    /// In case of ambiguity (can happen with multiple merges of the same branches) returns the
    /// common ancestor with lowest id out of those with highest generation number.
    pub async fn common_base_with(
        &self,
        other_commit: ChangesetId,
    ) -> Result<Option<ChangesetContext<R>>, MononokeError> {
        let lca = self
            .repo_ctx()
            .repo()
            .commit_graph()
            .common_base(self.ctx(), self.id, other_commit)
            .watched()
            .await?;
        Ok(lca.first().map(|id| Self::new(self.repo_ctx.clone(), *id)))
    }

    /// Returns the lowest common ancestor of two commits in the p1 linear graph, ignoring merge parents.
    pub async fn linear_common_base_with(
        &self,
        other_commit: ChangesetId,
    ) -> Result<Option<ChangesetContext<R>>, MononokeError> {
        let lca = self
            .repo_ctx()
            .repo()
            .commit_graph()
            .p1_linear_graph()
            .skip_tree_lowest_common_ancestor(self.ctx(), self.id, other_commit)
            .watched()
            .await?;
        Ok(lca.map(|node| Self::new(self.repo_ctx.clone(), node.cs_id)))
    }
}

impl<R: BonsaiHgMappingRef + RepoDerivedDataRef + RepoDerivedDataArc + RepoIdentityRef>
    ChangesetContext<R>
{
    /// The Mercurial ID for the changeset.
    pub async fn hg_id(&self) -> Result<Option<HgChangesetId>, MononokeError> {
        let maybe_hg_id = self
            .repo_ctx()
            .repo()
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(self.ctx(), self.id)
            .await?;
        let hg_id = if maybe_hg_id.is_none() && self.repo_ctx().derive_hgchangesets_enabled() {
            let mapped_hg_id = self.derive::<MappedHgChangesetId>().await?;
            Some(mapped_hg_id.hg_changeset_id())
        } else {
            maybe_hg_id
        };
        // Materialize the augmented manifest for the imminent `trees` fetch
        // (covers both the mapping-hit and derive-on-miss paths above). Skipped
        // when there's no hg changeset to derive one from.
        if hg_id.is_some() {
            self.repo_ctx()
                .ensure_hg_augmented_manifests_derived(Some(self.id))
                .await?;
        }
        Ok(hg_id)
    }
}

impl<R: BonsaiGitMappingRef + RepoDerivedDataRef + RepoDerivedDataArc> ChangesetContext<R> {
    /// The git Sha1 for the changeset (if available).
    pub async fn git_sha1(&self) -> Result<Option<GitSha1>, MononokeError> {
        let maybe_git_sha1 = self
            .repo_ctx()
            .repo()
            .bonsai_git_mapping()
            .get_git_sha1_from_bonsai(self.ctx(), self.id)
            .await?;
        if maybe_git_sha1.is_none() && self.repo_ctx().derive_gitcommit_enabled() {
            let mapped_git_commit_id = self.derive::<MappedGitCommitId>().await?;
            return Ok(Some(*mapped_git_commit_id.oid()));
        }
        Ok(maybe_git_sha1)
    }
}

impl<R: RepoDerivedDataRef + RepoDerivedDataArc + RepoBlobstoreArc> ChangesetContext<R> {
    /// Get the `ChangesetInfo` for this changeset.
    pub async fn changeset_info(&self) -> Result<ChangesetInfo, MononokeError> {
        if self.repo_ctx.derive_changeset_info_enabled() {
            self.changeset_info
                .get_or_init(|| self.derive::<ChangesetInfo>())
                .await
        } else {
            let bonsai = self.bonsai_changeset().await?;
            Ok(ChangesetInfo::new(self.id(), bonsai))
        }
    }

    /// The IDs of the parents of the changeset.
    pub async fn parents(&self) -> Result<Vec<ChangesetId>, MononokeError> {
        Ok(self.changeset_info().await?.parents().collect())
    }

    /// The author of the changeset.
    pub async fn author(&self) -> Result<String, MononokeError> {
        Ok(self.changeset_info().await?.author().to_string())
    }

    /// The date the changeset was authored.
    pub async fn author_date(&self) -> Result<DateTime<FixedOffset>, MononokeError> {
        Ok(self
            .changeset_info()
            .await?
            .author_date()
            .as_chrono()
            .clone())
    }

    /// The committer of the changeset.  May be `None` if the committer
    /// is not tracked.
    pub async fn committer(&self) -> Result<Option<String>, MononokeError> {
        Ok(self
            .changeset_info()
            .await?
            .committer()
            .map(|s| s.to_string()))
    }

    /// The date the changeset was committed.  May be `None` if the
    /// committer is not tracked.
    pub async fn committer_date(&self) -> Result<Option<DateTime<FixedOffset>>, MononokeError> {
        Ok(self
            .changeset_info()
            .await?
            .committer_date()
            .map(|d| d.as_chrono().clone()))
    }

    /// The commit message.
    pub async fn message(&self) -> Result<String, MononokeError> {
        Ok(self.changeset_info().await?.message().to_string())
    }

    /// All mercurial commit extras as (name, value) pairs.
    pub async fn hg_extras(&self) -> Result<Vec<(String, Vec<u8>)>, MononokeError> {
        Ok(self
            .changeset_info()
            .await?
            .hg_extra()
            .map(|(name, value)| (name.to_string(), Vec::from(value)))
            .collect())
    }

    pub async fn git_extra_headers(
        &self,
    ) -> Result<Option<Vec<(SmallVec<[u8; 24]>, Bytes)>>, MononokeError> {
        Ok(self
            .changeset_info()
            .await?
            .git_extra_headers()
            .map(|headers| {
                headers
                    .map(|(key, value)| (SmallVec::from(key), Bytes::copy_from_slice(value)))
                    .collect()
            }))
    }

    pub async fn subtree_change_count(&self) -> Result<usize, MononokeError> {
        Ok(self.changeset_info().await?.subtree_change_count())
    }
}

impl<R: RepoBlobstoreArc> ChangesetContext<R> {
    /// File changes associated with the commit.
    pub async fn file_changes(
        &self,
    ) -> Result<SortedVectorMap<NonRootMPath, FileChange>, MononokeError> {
        let bonsai = self.bonsai_changeset().await?;
        let bonsai = bonsai.into_mut();
        Ok(bonsai.file_changes)
    }

    pub async fn subtree_changes(
        &self,
    ) -> Result<SortedVectorMap<MPath, SubtreeChange>, MononokeError> {
        let bonsai = self.bonsai_changeset().await?;
        let bonsai = bonsai.into_mut();
        Ok(bonsai.subtree_changes)
    }
}

impl<R: RepoDerivedDataArc + RepoBlobstoreRef> ChangesetContext<R> {
    async fn find_entries(
        &self,
        prefixes: Option<Vec1<MPath>>,
        ordering: ChangesetFileOrdering,
    ) -> Result<
        impl Stream<Item = Result<(MPath, ManifestEntry<SkeletonManifestId, ()>), anyhow::Error>>
        + use<R>,
        MononokeError,
    > {
        let root = self.root_skeleton_manifest_id().await?;
        let prefixes = match prefixes {
            Some(prefixes) => prefixes.into_iter().map(PathOrPrefix::Prefix).collect(),
            None => vec![PathOrPrefix::Prefix(MPath::ROOT)],
        };
        let entries = match ordering {
            ChangesetFileOrdering::Unordered => root
                .skeleton_manifest_id()
                .find_entries(
                    self.ctx().clone(),
                    self.repo_ctx().repo().repo_blobstore().clone(),
                    prefixes,
                )
                .left_stream(),
            ChangesetFileOrdering::Ordered { after } => root
                .skeleton_manifest_id()
                .find_entries_ordered(
                    self.ctx().clone(),
                    self.repo_ctx().repo().repo_blobstore().clone(),
                    prefixes,
                    after,
                )
                .right_stream(),
        };
        Ok(entries)
    }

    async fn find_entries_v2(
        &self,
        prefixes: Option<Vec1<MPath>>,
        ordering: ChangesetFileOrdering,
    ) -> Result<
        impl Stream<Item = Result<(MPath, ManifestEntry<SkeletonManifestV2, ()>), anyhow::Error>>
        + use<R>,
        MononokeError,
    > {
        let root = self.root_skeleton_manifest_v2_id().await?;
        let manifest = root
            .inner_id()
            .load(self.ctx(), self.repo_ctx().repo().repo_blobstore())
            .await?;
        let prefixes = match prefixes {
            Some(prefixes) => prefixes.into_iter().map(PathOrPrefix::Prefix).collect(),
            None => vec![PathOrPrefix::Prefix(MPath::ROOT)],
        };
        let entries = match ordering {
            ChangesetFileOrdering::Unordered => manifest
                .find_entries(
                    self.ctx().clone(),
                    self.repo_ctx().repo().repo_blobstore().clone(),
                    prefixes,
                )
                .left_stream(),
            ChangesetFileOrdering::Ordered { after } => manifest
                .find_entries_ordered(
                    self.ctx().clone(),
                    self.repo_ctx().repo().repo_blobstore().clone(),
                    prefixes,
                    after,
                )
                .right_stream(),
        };
        Ok(entries)
    }
}

impl<R: CommitGraphRef + RepoDerivedDataRef + RepoBlobstoreRef + Clone + Send + Sync + 'static>
    ChangesetContext<R>
{
    /// Returns a stream of `ChangesetContext` for the history of the repository from this commit.
    pub async fn history(
        &self,
        opts: ChangesetHistoryOptions,
    ) -> Result<BoxStream<'_, Result<ChangesetContext<R>, MononokeError>>, MononokeError> {
        let mut ancestors_stream_builder = AncestorsStreamBuilder::new(
            Arc::new(self.repo_ctx().repo().commit_graph().parents_graph()),
            self.ctx().clone(),
            vec![self.id()],
        );

        if opts.until_timestamp.is_some() || opts.until_committer_timestamp.is_some() {
            ancestors_stream_builder = ancestors_stream_builder.with({
                let ctx = self.ctx().clone();
                let repo_ctx = self.repo_ctx().clone();
                let cs_info_enabled = repo_ctx.derive_changeset_info_enabled();
                move |cs_id| {
                    cloned!(ctx, repo_ctx, cs_info_enabled);
                    async move {
                        let info = if cs_info_enabled {
                            repo_ctx
                                .repo()
                                .repo_derived_data()
                                .derive::<ChangesetInfo>(&ctx, cs_id, DerivationPriority::LOW)
                                .await?
                        } else {
                            let bonsai = cs_id.load(&ctx, repo_ctx.repo().repo_blobstore()).await?;
                            ChangesetInfo::new(cs_id, bonsai)
                        };

                        if let Some(until_timestamp) = opts.until_timestamp {
                            let date = info.author_date().as_chrono().clone();
                            if date.timestamp() < until_timestamp {
                                return Ok(false);
                            }
                        }
                        if let Some(until_committer_timestamp) = opts.until_committer_timestamp {
                            // Get committer_date if available, otherwise fall back to author_date
                            let date = match info.committer_date() {
                                Some(committer_date) => committer_date.as_chrono().clone(),
                                None => info.author_date().as_chrono().clone(),
                            };
                            if date.timestamp() < until_committer_timestamp {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                }
            });
        }

        if let Some(descendants_of) = opts.descendants_of {
            ancestors_stream_builder = ancestors_stream_builder.descendants_of(descendants_of);
        }

        if let Some(exclude_changeset_and_ancestors) = opts.exclude_changeset_and_ancestors {
            ancestors_stream_builder = ancestors_stream_builder
                .exclude_ancestors_of(vec![exclude_changeset_and_ancestors]);
        }

        let cs_ids_stream = ancestors_stream_builder.build().await?;

        Ok(cs_ids_stream
            .map_err(MononokeError::from)
            .and_then(move |cs_id| async move {
                Ok::<_, MononokeError>(ChangesetContext::new(self.repo_ctx().clone(), cs_id))
            })
            .boxed())
    }
}

impl<R: CommitGraphArc + Clone + Send + Sync + 'static> ChangesetContext<R> {
    pub async fn linear_history(
        &self,
        opts: ChangesetLinearHistoryOptions,
    ) -> Result<BoxStream<'_, Result<ChangesetContext<R>, MononokeError>>, MononokeError> {
        let mut linear_ancestors_stream_builder = LinearAncestorsStreamBuilder::new(
            self.repo_ctx().repo().commit_graph_arc(),
            self.ctx().clone(),
            self.id(),
        )
        .await?;

        if let Some(exclude_changeset_and_ancestors) = opts.exclude_changeset_and_ancestors {
            linear_ancestors_stream_builder = linear_ancestors_stream_builder
                .exclude_ancestors_of(exclude_changeset_and_ancestors)
                .await?;
        }

        if let Some(descendants_of) = opts.descendants_of {
            linear_ancestors_stream_builder = linear_ancestors_stream_builder
                .descendants_of(descendants_of)
                .await?;
        }

        linear_ancestors_stream_builder = linear_ancestors_stream_builder.skip(opts.skip);

        let cs_ids_stream = linear_ancestors_stream_builder.build().await?;

        Ok(cs_ids_stream
            .map_err(MononokeError::from)
            .and_then(move |cs_id| async move {
                Ok::<_, MononokeError>(ChangesetContext::new(self.repo_ctx().clone(), cs_id))
            })
            .boxed())
    }
}

/// Merge a sorted `supplement` of `ChangesetPathDiffContext` entries into a
/// `manifest_vec`. The two inputs are disjoint by construction in the
/// LFS-renormalize use case (manifest emits only when content_id or
/// file_type differs; the supplement requires they match), so no tie-break
/// is needed.
///
/// Ordered: classic two-iterator sort-merge by `.path()`. Both inputs must
/// already be sorted ascending by path.
///
/// Unordered: append `supplement` to `manifest_vec` (no ordering guarantee
/// across the join).
pub(crate) fn insert_sorted_results<R>(
    manifest_vec: &mut Vec<ChangesetPathDiffContext<R>>,
    supplement: Vec<ChangesetPathDiffContext<R>>,
    is_ordered: bool,
) {
    if supplement.is_empty() {
        return;
    }
    if !is_ordered {
        manifest_vec.extend(supplement);
        return;
    }

    *manifest_vec = std::mem::take(manifest_vec)
        .into_iter()
        .merge_by(supplement, |m, s| m.path() <= s.path())
        .collect();
}

impl<R: MononokeRepo> ChangesetContext<R> {
    /// Query the root directory in the repository at this changeset revision.
    pub async fn root(&self) -> Result<ChangesetPathContentContext<R>, MononokeError> {
        ChangesetPathContentContext::new(self.clone(), None).await
    }

    /// Returns `true` if this changeset is public (landed and reachable from a public bookmark).
    pub async fn is_public(&self) -> Result<bool, MononokeError> {
        let public = self
            .repo_ctx()
            .repo()
            .phases()
            .get_cached_public(self.ctx(), vec![self.id])
            .await?;
        Ok(public.contains(&self.id))
    }
}

impl<R> ChangesetContext<R>
where
    R: RepoPermissionCheckerRef
        + AclRegionsRef
        + RepoIdentityRef
        + RestrictedPathsArc
        + RepoBlobstoreArc
        + RepoBlobstoreRef
        + RepoDerivedDataArc
        + RepoDerivedDataRef
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Query a path within the repository. This could be a file or a
    /// directory.
    ///
    /// Returns a path content context, which is a context suitable for
    /// queries about the content at this path.
    pub async fn path_with_content<P>(
        &self,
        path: P,
    ) -> Result<ChangesetPathContentContext<R>, MononokeError>
    where
        P: TryInto<MPath>,
        P::Error: Display,
    {
        ChangesetPathContentContext::new(
            self.clone(),
            path.try_into()
                .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?,
        )
        .await
    }

    /// How a requested path resolved under partial-response semantics.
    pub async fn check_path_visibility(
        &self,
        path: &MPath,
    ) -> Result<PathVisibility<R>, MononokeError> {
        match self.path_with_content(path.clone()).await {
            Ok(ctx) => Ok(PathVisibility::Present(ctx)),
            Err(e) => match e.restricted_paths_denial() {
                Some(denial) => Ok(PathVisibility::Denied(denial.denying_restriction().clone())),
                None => Err(e),
            },
        }
    }
}

impl<R: MononokeRepo> ChangesetContext<R> {
    /// Query a path within the repository. This could be a file or a
    /// directory.
    ///
    /// Returns a path history context, which is a context suitable for
    /// queries about the history of this path.
    pub async fn path_with_history<P>(
        &self,
        path: P,
    ) -> Result<ChangesetPathHistoryContext<R>, MononokeError>
    where
        P: TryInto<MPath>,
        P::Error: Display,
    {
        ChangesetPathHistoryContext::new(
            self.clone(),
            path.try_into()
                .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?,
        )
        .await
    }

    /// Query a path within the repository. This could be a file or a
    /// directory.
    ///
    /// Returns a path context, which is a context that is only suitable for
    /// queries about the type of item that exists at this path.
    ///
    /// If you need to query the content or history of a path, use
    /// `path_with_content` or `path_with_history` instead.
    pub async fn path<P>(&self, path: P) -> Result<ChangesetPathContext<R>, MononokeError>
    where
        P: TryInto<MPath>,
        P::Error: Display,
    {
        ChangesetPathContext::new(
            self.clone(),
            path.try_into()
                .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?,
        )
        .await
    }

    /// Returns a stream of path history contexts for a set of paths.
    ///
    /// This performs an efficient manifest traversal, and as such returns
    /// contexts only for **paths which exist**.
    ///
    /// How restricted-path denials are handled is governed by `policy`:
    /// `Strict` fails the stream on any denial, `SkipAndCollect` skips
    /// denied paths, recording each against its denying restriction.
    pub async fn paths_with_history<T: Iterator<Item = MPath>>(
        &self,
        paths: T,
        policy: &RestrictedPathsPolicy,
    ) -> Result<
        impl Stream<Item = Result<ChangesetPathHistoryContext<R>, MononokeError>> + use<R, T>,
        MononokeError,
    > {
        let stream = self
            .root_unode_manifest_id()
            .await?
            .manifest_unode_id()
            .find_entries(
                self.ctx().clone(),
                self.repo_ctx().repo().repo_blobstore().clone(),
                paths,
            )
            .map_err(MononokeError::from)
            .and_then({
                let changeset = self.clone();
                move |(mpath, entry)| {
                    ChangesetPathHistoryContext::new_with_unode_entry(
                        changeset.clone(),
                        mpath,
                        entry,
                    )
                }
            });
        match policy {
            RestrictedPathsPolicy::Strict => Ok(stream.left_stream()),
            RestrictedPathsPolicy::SkipAndCollect(collector) => {
                Ok(skip_restricted_denials(stream, collector.clone()).right_stream())
            }
        }
    }

    /// Returns a stream of path content contexts for a set of paths.
    ///
    /// This performs an efficient manifest traversal, and as such returns
    /// contexts only for **paths which exist**.
    ///
    /// How restricted-path denials are handled is governed by `policy`:
    /// `Strict` fails the stream on any denial, `SkipAndCollect` skips
    /// denied paths, recording each against its denying restriction.
    pub async fn paths_with_content<T: Iterator<Item = MPath>>(
        &self,
        paths: T,
        policy: &RestrictedPathsPolicy,
    ) -> Result<
        impl Stream<Item = Result<ChangesetPathContentContext<R>, MononokeError>> + use<R, T>,
        MononokeError,
    > {
        let root_id = self.root_content_manifest_id().await?;

        let stream = root_id
            .find_entries(
                self.ctx().clone(),
                self.repo_ctx().repo().repo_blobstore().clone(),
                paths,
            )
            .map_err(MononokeError::from)
            .and_then({
                let changeset = self.clone();
                move |(mpath, entry)| {
                    cloned!(changeset);
                    async move {
                        ChangesetPathContentContext::new_with_manifest_entry(
                            changeset.clone(),
                            mpath,
                            entry,
                        )
                        .await
                    }
                }
            });
        match policy {
            RestrictedPathsPolicy::Strict => Ok(stream.left_stream()),
            RestrictedPathsPolicy::SkipAndCollect(collector) => {
                Ok(skip_restricted_denials(stream, collector.clone()).right_stream())
            }
        }
    }

    /// Returns a stream of path contexts for a set of paths.
    ///
    /// This performs an efficient manifest traversal, and as such returns
    /// contexts only for **paths which exist**.
    pub async fn paths<T: Iterator<Item = MPath>>(
        &self,
        paths: T,
    ) -> Result<
        impl Stream<Item = Result<ChangesetPathContext<R>, MononokeError>> + use<R, T>,
        MononokeError,
    > {
        if justknobs::eval(
            "scm/mononoke:changeset_path_context_use_skeleton_manifest_v2",
            None,
            Some(self.repo_ctx().name()),
        ) {
            Ok(self
                .root_skeleton_manifest_v2_id()
                .await?
                .into_inner_id()
                .load(self.ctx(), self.repo_ctx().repo().repo_blobstore())
                .await?
                .find_entries(
                    self.ctx().clone(),
                    self.repo_ctx().repo().repo_blobstore().clone(),
                    paths,
                )
                .map_err(MononokeError::from)
                .and_then({
                    let changeset = self.clone();
                    move |(mpath, entry)| {
                        ChangesetPathContext::new_with_entry(
                            changeset.clone(),
                            mpath,
                            entry.map_tree(|_| ()),
                        )
                    }
                })
                .left_stream())
        } else {
            Ok(self
                .root_skeleton_manifest_id()
                .await?
                .skeleton_manifest_id()
                .find_entries(
                    self.ctx().clone(),
                    self.repo_ctx().repo().repo_blobstore().clone(),
                    paths,
                )
                .map_err(MononokeError::from)
                .and_then({
                    let changeset = self.clone();
                    move |(mpath, entry)| {
                        ChangesetPathContext::new_with_entry(
                            changeset.clone(),
                            mpath,
                            entry.map_tree(|_| ()),
                        )
                    }
                })
                .right_stream())
        }
    }

    fn deleted_paths_impl<Root: RootDeletedManifestIdCommon>(
        &self,
        root: Root,
        paths: impl Iterator<Item = MPath> + 'static,
    ) -> impl Stream<Item = Result<ChangesetPathHistoryContext<R>, MononokeError>> + '_ {
        root.find_entries(self.ctx(), self.repo_ctx().repo().repo_blobstore(), paths)
            .map_err(MononokeError::from)
            .and_then({
                let changeset = self.clone();
                move |(mpath, entry)| {
                    ChangesetPathHistoryContext::new_with_deleted_manifest::<Root::Manifest>(
                        changeset.clone(),
                        mpath,
                        entry,
                    )
                }
            })
    }

    /// Returns a stream of path history contexts for a set of paths.
    ///
    /// This performs an efficient manifest traversal, and as such returns
    /// contexts only for **deleted paths which have existed previously**.
    ///
    /// How restricted-path denials are handled is governed by `policy`:
    /// `Strict` fails the stream on any denial, `SkipAndCollect` skips
    /// denied paths, recording each against its denying restriction.
    pub async fn deleted_paths(
        &self,
        paths: impl Iterator<Item = MPath> + 'static,
        policy: &RestrictedPathsPolicy,
    ) -> Result<
        impl Stream<Item = Result<ChangesetPathHistoryContext<R>, MononokeError>> + '_,
        MononokeError,
    > {
        let stream = self.deleted_paths_impl(self.root_deleted_manifest_v2_id().await?, paths);
        match policy {
            RestrictedPathsPolicy::Strict => Ok(stream.left_stream()),
            RestrictedPathsPolicy::SkipAndCollect(collector) => {
                Ok(skip_restricted_denials(stream, collector.clone()).right_stream())
            }
        }
    }
}

impl<R> ChangesetContext<R>
where
    R: RepoPermissionCheckerRef
        + AclRegionsRef
        + RepoIdentityRef
        + RepoConfigRef
        + RestrictedPathsArc
        + RepoBlobstoreRef
        + RepoBlobstoreArc
        + RepoDerivedDataRef
        + RepoDerivedDataArc
        + RepoEphemeralStoreRef
        + RepoWithBubble
        + CommitGraphRef
        + CommitGraphArc
        + BonsaiHgMappingRef
        + BonsaiGitMappingRef
        + BonsaiGlobalrevMappingRef
        + BonsaiSvnrevMappingRef
        + BonsaiP4MappingRef
        + MutableRenamesArc
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub async fn diff_unordered(
        &self,
        other: &ChangesetContext<R>,
        include_copies_renames: bool,
        include_subtree_copies: bool,
        path_restrictions: Option<Vec<MPath>>,
        diff_items: BTreeSet<ChangesetDiffItem>,
        policy: &RestrictedPathsPolicy,
    ) -> Result<Vec<ChangesetPathDiffContext<R>>, MononokeError> {
        self.diff(
            other,
            include_copies_renames,
            include_subtree_copies,
            path_restrictions,
            diff_items,
            ChangesetFileOrdering::Unordered,
            None,
            policy,
        )
        .watched()
        .await
    }

    /// Find paths whose only change between `self` and `other` is a
    /// Git-LFS discriminator flip (raw blob ↔ canonical pointer, same
    /// resolved content). The manifest walk in `diff()` misses these
    /// because manifest leaves carry no `git_lfs` field.
    ///
    /// Candidates are `FileChange::Change` entries in `self`'s bonsai
    /// that the manifest walk did not emit; by construction, such a
    /// candidate matches `other` on `(content_id, file_type)`, so the
    /// only field that can differ is `git_lfs`. We do not inspect
    /// `other`'s bonsai — the orphan-pointer cleanup workflow inherits
    /// the LFS state from an ancestor, so neither side records it.
    ///
    /// Pass `excluded_paths` as the set already emitted by the manifest
    /// walk; this keeps the returned set disjoint from it without
    /// re-deriving manifests.
    ///
    /// Assumes producers only emit `FileChange::Change` for real changes
    /// (`emits_re_recorded_state` locks the no-op-Change case: we
    /// surface, never mask) and that candidates have `copy_from = None`
    /// (gitimport hardcodes this; if a future producer changes it,
    /// filter `tracked.copy_from().is_some()` here).
    ///
    /// Callers must pass `self`'s first parent for `other` and a repo
    /// with `git_lfs_interpret_pointers`, or this returns empty. Returns
    /// `ChangesetPathDiffContext` entries sorted ascending by path,
    /// lazy-loaded so path-only consumers pay only per-path ACL cost.
    pub(crate) async fn get_potential_lfs_changes(
        &self,
        other: &ChangesetContext<R>,
        path_restrictions: &Option<Vec<MPath>>,
        subtree_copy_sources: &manifest::PathTree<Option<(ChangesetContext<R>, MPath)>>,
        excluded_paths: &HashSet<MPath>,
        after: Option<&MPath>,
        before_or_at: Option<&MPath>,
        max_entries: usize,
        policy: &RestrictedPathsPolicy,
    ) -> Result<Vec<ChangesetPathDiffContext<R>>, MononokeError> {
        // Per-path `ChangesetPathContentContext::new` calls do an ACL check
        // and a restricted-paths check. Bound the parallel fan-out per the
        // project guidance for path-context construction.
        const LFS_PATH_CONTEXT_CONCURRENCY: usize = 50;

        if max_entries == 0
            || !self
                .repo_ctx()
                .repo()
                .repo_config()
                .git_configs
                .git_lfs_interpret_pointers
        {
            return Ok(Vec::new());
        }

        if before_or_at.is_some_and(|b| b.is_root() || after.is_some_and(|a| b <= a)) {
            return Ok(Vec::new());
        }

        fn within_restrictions(path: &MPath, path_restrictions: &Option<Vec<MPath>>) -> bool {
            path_restrictions.as_ref().is_none_or(|i| {
                i.iter()
                    .any(|path_restriction| path.is_related_to(path_restriction))
            })
        }

        let self_changes = self.file_changes().await?;

        let start_bound = after
            .cloned()
            .and_then(MPath::into_optional_non_root_path)
            .map_or(Bound::Unbounded, Bound::Excluded);
        let end_bound = before_or_at
            .cloned()
            .and_then(MPath::into_optional_non_root_path)
            .map_or(Bound::Unbounded, Bound::Included);

        let candidates: Vec<MPath> = self_changes
            .range::<NonRootMPath, _>((start_bound, end_bound))
            .filter_map(|(non_root_path, file_change)| {
                let FileChange::Change(_) = file_change else {
                    return None;
                };

                let path: MPath = non_root_path.clone().into();
                if !within_restrictions(&path, path_restrictions) {
                    return None;
                }

                // Skip paths inside a subtree-copy region: for those, the
                // correct comparison side is the copy source, not `other`,
                // and the main manifest walk handles them via its own
                // subtree-copy logic.
                if subtree_copy_sources
                    .get_nearest_parent(&path, Option::is_some)
                    .is_some()
                {
                    return None;
                }

                // Already emitted by the manifest walk -- the manifests
                // differed on content_id or file_type, so it isn't an
                // LFS-only flip.
                if excluded_paths.contains(&path) {
                    return None;
                }

                Some(path)
            })
            .collect();

        stream::iter(candidates)
            .map(|path| async move {
                let (new_side, old_side) = try_join(
                    policy_side(
                        ChangesetPathContentContext::new(self.clone(), path.clone()),
                        policy,
                    ),
                    policy_side(
                        ChangesetPathContentContext::new(other.clone(), path.clone()),
                        policy,
                    ),
                )
                .await?;
                if new_side.is_none() && old_side.is_none() {
                    Ok(None)
                } else {
                    ChangesetPathDiffContext::new_file(
                        self.clone(),
                        path,
                        new_side,
                        old_side,
                        CopyInfo::None,
                        None,
                    )
                    .map(Some)
                }
            })
            .buffered(LFS_PATH_CONTEXT_CONCURRENCY)
            .try_filter_map(|opt| futures::future::ready(Ok(opt)))
            .take(max_entries)
            .try_collect()
            .await
    }

    /// If the given path is part of a subtree copy, get the subtree copy source, as well as the replacement path
    /// from the subtree copy destination.
    ///
    /// If the path was not part of a subtree copy, return the changeset and path as-is.
    fn get_subtree_copy_source(
        subtree_copy_sources: &manifest::PathTree<Option<(ChangesetContext<R>, MPath)>>,
        changeset: &ChangesetContext<R>,
        path: &MPath,
    ) -> Result<(ChangesetContext<R>, MPath, Option<MPath>), MononokeError> {
        if let Some((dest_path, Some((source_cs, source_path)))) =
            subtree_copy_sources.get_nearest_parent(path, Option::is_some)
        {
            Ok((
                source_cs.clone(),
                path.reparent(&dest_path, source_path)?,
                Some(path.clone()),
            ))
        } else {
            Ok((changeset.clone(), path.clone(), None))
        }
    }

    /// Returns differences between this changeset and some other changeset.
    ///
    /// `self` is considered the "new" changeset (so files missing there are "Removed")
    /// `other` is considered the "old" changeset (so files missing there are "Added")
    /// `include_copies_renames` and `include_subtree_copies` are only available
    /// when diffing commits with its parent
    /// `path_restrictions` if present will narrow down the diff to given paths
    /// `diff_items` what to include in the output (files, dirs or both)
    ///
    /// How restricted-path denials are handled is governed by `policy`:
    /// `Strict` fails the whole diff on any denial; `SkipAndCollect` applies
    /// the per-side absent rule instead (a denied new side downgrades
    /// `Changed` to `Removed` and drops copy info from `Added`, a denied old
    /// side downgrades `Changed` to `Added`, an entry denied on both sides is
    /// omitted), recording denials against their denying restriction.
    /// Repo-auth denials and all other errors still fail the whole diff.
    pub async fn diff(
        &self,
        other: &ChangesetContext<R>,
        include_copies_renames: bool,
        include_subtree_copies: bool,
        path_restrictions: Option<Vec<MPath>>,
        diff_items: BTreeSet<ChangesetDiffItem>,
        ordering: ChangesetFileOrdering,
        limit: Option<usize>,
        policy: &RestrictedPathsPolicy,
    ) -> Result<Vec<ChangesetPathDiffContext<R>>, MononokeError>
    where
        R: RepoConfigRef,
    {
        // Helper to that checks if a path is within the given path restrictions
        fn within_restrictions(path: &MPath, path_restrictions: &Option<Vec<MPath>>) -> bool {
            path_restrictions.as_ref().is_none_or(|i| {
                i.iter()
                    .any(|path_restriction| path.is_related_to(path_restriction))
            })
        }

        // map from from_path to to_paths (there may be multiple copies
        // for each from_path, so this maps to a vector of paths)
        let mut copy_path_map = HashMap::new();
        // map from to_path to from_path
        let mut inv_copy_path_map = HashMap::new();
        let mut manifest_replacements = HashMap::new();
        let mut subtree_copy_sources = manifest::PathTree::default();
        let parents = self.parents().await?;
        // We can only consider copies or subtree copies when comparing with a parent
        let comparing_against_parent = parents.contains(&other.id);
        // The bonsai-only LFS supplement is only valid when `other` is `self`'s
        // first parent; this matches the common-case `commit_compare` usage and
        // avoids emitting spurious entries for merge changesets compared
        // against a non-leftmost parent.
        let first_parent_is_other = parents.first() == Some(&other.id);
        if comparing_against_parent && include_copies_renames {
            let file_changes = self.file_changes().await?;
            if let Some(overrides) = &self.mutable_history {
                for (dst_path, mutable_history) in overrides {
                    if let Some((cs_id, path)) = mutable_history.get_copy_from() {
                        if cs_id == other.id() {
                            copy_path_map
                                .entry(path.clone())
                                .or_insert_with(Vec::new)
                                .push(dst_path.clone());
                        }
                    }
                }
            }

            for (to_path, file_change) in file_changes.iter() {
                let to_path: MPath = to_path.clone().into();
                let path_is_overridden = self
                    .mutable_history
                    .as_ref()
                    .and_then(|history_map| {
                        history_map
                            .get(&to_path)
                            .map(PathMutableHistory::is_override)
                    })
                    .unwrap_or(false);
                if path_is_overridden {
                    // Mutable history overrides immutable if present
                    continue;
                }
                match file_change {
                    FileChange::Change(tc) => {
                        if let Some((from_path, csid)) = tc.copy_from() {
                            let from_path: MPath = from_path.clone().into();
                            if *csid == other.id {
                                copy_path_map
                                    .entry(from_path)
                                    .or_insert_with(Vec::new)
                                    .push(to_path);
                            }
                        }
                    }
                    FileChange::Deletion
                    | FileChange::UntrackedDeletion
                    | FileChange::UntrackedChange(_) => {}
                }
            }

            // Copy detection below looks up every copy source and destination,
            // so keep only the entries the diff can read. The diff reads the
            // copy maps for an added file within the restrictions (through
            // its source's entry) and for a removed file within the
            // restrictions (as a source). So every entry it reads has its
            // source or a destination within the restrictions. An entry's
            // outcome does not depend on other entries, and a kept entry keeps
            // all its destinations in order, because the first one that did
            // not exist in the parent is the move and the rest are copies.
            if path_restrictions.is_some() {
                copy_path_map.retain(|from_path, to_paths| {
                    iter::once(from_path)
                        .chain(to_paths.iter())
                        .any(|path| within_restrictions(path, &path_restrictions))
                });
            }
            let to_paths: HashSet<MPath> = copy_path_map.values().flatten().cloned().collect();

            let other_root_manifest_id = other.root_content_manifest_id().await?;

            // Prefetch manifest entries for all "from paths" so that we don't
            // need to refetch them later.  Use other's blobstore since we're
            // accessing other's manifest data.
            let from_path_to_mf_entry = other_root_manifest_id
                .find_entries(
                    self.ctx().clone(),
                    other.repo_ctx().repo().repo_blobstore().clone(),
                    copy_path_map.keys().cloned(),
                )
                .try_collect::<HashMap<_, _>>();

            // At the same time, find out whether the destinations of copies
            // already existed in the parent.
            let to_path_exists_in_parent = other_root_manifest_id
                .find_entries(
                    self.ctx().clone(),
                    other.repo_ctx().repo().repo_blobstore().clone(),
                    to_paths,
                )
                .map_ok(|(path, _)| path)
                .try_collect::<HashSet<_>>();

            let (from_path_to_mf_entry, to_path_exists_in_parent) =
                try_join(from_path_to_mf_entry, to_path_exists_in_parent).await?;

            // Filter out:
            // - Copies where the to_path already existed in the parent.  These don't show up as
            //   copies in the diff view.
            // - Copies where the from_path didn't exist in the parent.  These are indicative of
            //   an invalid bonsai changeset or mutable rename and can be ignored.
            copy_path_map.retain(|from_path, to_paths| {
                to_paths.retain(|to_path| !to_path_exists_in_parent.contains(to_path));
                !to_paths.is_empty() && from_path_to_mf_entry.contains_key(from_path)
            });

            // Build the inverse copy map (from to_path to from_path),
            // which includes the manifest entry for the from_path.
            for (from_path, to_paths) in copy_path_map.iter() {
                let mf_entry = from_path_to_mf_entry.get(from_path).ok_or_else(|| {
                    MononokeError::from(anyhow!(
                        "internal error: cannot find {from_path:?} in parent commit"
                    ))
                })?;
                for to_path in to_paths {
                    inv_copy_path_map.insert(to_path, (from_path, mf_entry.clone()));
                }
            }
        }
        if comparing_against_parent && include_subtree_copies {
            let subtree_changes = self.subtree_changes().await?;
            let repo_ctx = self.repo_ctx().clone();
            let ctx = self.ctx().clone();
            let blobstore = self.repo_ctx().repo().repo_blobstore().clone();
            let results: Vec<_> = stream::iter(subtree_changes.into_iter().filter_map(
                |(path, change)| {
                    change
                        .deep_copy_source()
                        .map(|(from_cs_id, from_path)| (path, from_cs_id, from_path.clone()))
                },
            ))
            .map(|(path, from_cs_id, from_path)| {
                cloned!(repo_ctx, ctx, blobstore);
                async move {
                    let from_cs =
                        repo_ctx
                            .changeset(from_cs_id)
                            .await?
                            .ok_or_else(|| {
                                MononokeError::from(anyhow!(
                                    "Subtree copy source {from_cs_id} not found"
                                ))
                            })?;
                    let entry = from_cs
                        .root_content_manifest_id()
                        .await?
                        .find_entry(
                            ctx,
                            blobstore,
                            from_path.clone(),
                        )
                        .await?
                        .ok_or_else(|| {
                            MononokeError::from(anyhow!(
                                "Invalid subtree copy source: {from_cs_id} does not contain {from_path}"
                            ))
                        })?;
                    Ok::<_, MononokeError>((path, from_cs, from_path, entry))
                }
            })
            .buffer_unordered(10)
            .try_collect()
            .await?;
            for (path, from_cs, from_path, entry) in results {
                subtree_copy_sources.insert(path.clone(), Some((from_cs, from_path.clone())));
                manifest_replacements.insert(path, entry);
            }
        }

        // set of paths from other that were copied in (not moved)
        // We check if `self` contains paths that were source for copy or move in `other`
        // If self does contain a path, then we consider it to be a copy, otherwise
        // it's a move to the first location it was copied to.
        let copied_paths = self
            .root_content_manifest_id()
            .await?
            .find_entries(
                self.ctx().clone(),
                self.repo_ctx().repo().repo_blobstore().clone(),
                copy_path_map.keys().cloned(),
            )
            .map_ok(|(from_path, _)| from_path)
            .try_collect::<HashSet<_>>()
            .await?;

        let diff_files = diff_items.contains(&ChangesetDiffItem::FILES);
        let diff_trees = diff_items.contains(&ChangesetDiffItem::TREES);

        let (self_root, other_root) = try_join(
            self.root_content_manifest_id(),
            other.root_content_manifest_id(),
        )
        .await?;

        let recurse_pruner = {
            cloned!(path_restrictions);
            move |tree_diff: &ManifestDiff<_>| match tree_diff {
                ManifestDiff::Added(path, ..)
                | ManifestDiff::Changed(path, ..)
                | ManifestDiff::Removed(path, ..) => within_restrictions(path, &path_restrictions),
            }
        };

        let is_ordered = matches!(ordering, ChangesetFileOrdering::Ordered { .. });
        let supplement_after: Option<MPath> = match &ordering {
            ChangesetFileOrdering::Ordered { after } => after.clone(),
            ChangesetFileOrdering::Unordered => None,
        };

        let diff = match ordering {
            ChangesetFileOrdering::Unordered => {
                // We start from "other" as manifest.diff() is backwards.
                // Use other's blobstore for other_root data,
                // and self's blobstore for self_root data.
                other_root
                    .filtered_diff(
                        self.ctx().clone(),
                        other.repo_ctx().repo().repo_blobstore().clone(),
                        self_root,
                        self.repo_ctx().repo().repo_blobstore().clone(),
                        Some,
                        recurse_pruner,
                        manifest_replacements,
                    )
                    .left_stream()
            }
            ChangesetFileOrdering::Ordered { after } => {
                // We must find the weights of manifest replacements.
                let weighted_replacements = stream::iter(manifest_replacements)
                    .map(|(path, entry)| {
                        Ok(async move {
                            match entry {
                                ManifestEntry::Tree(manifest_id) => {
                                    let manifest = manifest_id
                                        .load(self.ctx(), self.repo_ctx().repo().repo_blobstore())
                                        .await?;
                                    let counts =
                                        manifest.subentries.rollup_data().descendant_counts;
                                    let weight = counts.files_count + counts.dirs_count;
                                    anyhow::Ok((
                                        path,
                                        ManifestEntry::Tree((weight as usize, manifest_id)),
                                    ))
                                }
                                ManifestEntry::Leaf(leaf) => Ok((path, ManifestEntry::Leaf(leaf))),
                            }
                        })
                    })
                    .try_buffered(10)
                    .try_collect()
                    .await?;

                // We start from "other" as manifest.diff() is backwards.
                // Use other's blobstore for other_root data,
                // and self's blobstore for self_root data.
                other_root
                    .filtered_diff_ordered(
                        self.ctx().clone(),
                        other.repo_ctx().repo().repo_blobstore().clone(),
                        self_root,
                        self.repo_ctx().repo().repo_blobstore().clone(),
                        after,
                        Some,
                        recurse_pruner,
                        weighted_replacements,
                    )
                    .right_stream()
            }
        };

        let mut change_contexts: Vec<ChangesetPathDiffContext<R>> = diff
            .map_err(MononokeError::from)
            .try_filter_map(|diff_entry| {
                async {
                    let entry = match diff_entry {
                        ManifestDiff::Added(path, entry @ ManifestEntry::Leaf(_)) => {
                            if !diff_files || !within_restrictions(&path, &path_restrictions) {
                                None
                            } else if let Some((from_path, from_entry)) =
                                inv_copy_path_map.get(&path)
                            {
                                // There's copy information that we can use.
                                let copy_info = if copied_paths.contains(from_path)
                                    || copy_path_map
                                        .get(from_path)
                                        .and_then(|to_paths| to_paths.first())
                                        != Some(&path)
                                {
                                    // If the source still exists in the current
                                    // commit, or this isn't the first place it
                                    // was copied to, it was a copy.
                                    CopyInfo::Copy
                                } else {
                                    // If it doesn't, and this is the first place
                                    // it was copied to, it was a move.
                                    CopyInfo::Move
                                };

                                let from = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        other.clone(),
                                        (*from_path).clone(),
                                        from_entry.clone(),
                                    ),
                                    policy,
                                )
                                .await?;
                                let new_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        self.clone(),
                                        path.clone(),
                                        entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                let Some(new_ctx) = new_side else {
                                    // New side denied: nothing visible to show.
                                    return Ok(None);
                                };
                                // Copy-from denied: hide provenance, show a
                                // plain Added rather than leaking the source.
                                let (from_opt, copy_info) = match from {
                                    Some(from) => (Some(from), copy_info),
                                    None => (None, CopyInfo::None),
                                };
                                Some(ChangesetPathDiffContext::new_file(
                                    self.clone(),
                                    path,
                                    Some(new_ctx),
                                    from_opt,
                                    copy_info,
                                    None,
                                )?)
                            } else {
                                let new_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        self.clone(),
                                        path.clone(),
                                        entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                match new_side {
                                    Some(new_ctx) => Some(ChangesetPathDiffContext::new_file(
                                        self.clone(),
                                        path,
                                        Some(new_ctx),
                                        None,
                                        CopyInfo::None,
                                        None,
                                    )?),
                                    // New side denied: nothing visible to show.
                                    None => None,
                                }
                            }
                        }
                        ManifestDiff::Removed(path, entry @ ManifestEntry::Leaf(_)) => {
                            #[allow(clippy::if_same_then_else)]
                            if copy_path_map.contains_key(&path) {
                                // The file is was moved (not removed), it will be covered by a "Moved" entry.
                                None
                            } else if !diff_files || !within_restrictions(&path, &path_restrictions)
                            {
                                None
                            } else {
                                let (source, source_path, replacement_path) =
                                    Self::get_subtree_copy_source(
                                        &subtree_copy_sources,
                                        other,
                                        &path,
                                    )?;
                                let old_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        source,
                                        source_path,
                                        entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                match old_side {
                                    Some(old_ctx) => Some(ChangesetPathDiffContext::new_file(
                                        self.clone(),
                                        path,
                                        None,
                                        Some(old_ctx),
                                        CopyInfo::None,
                                        replacement_path,
                                    )?),
                                    // Visible side denied: nothing to show.
                                    None => None,
                                }
                            }
                        }
                        ManifestDiff::Changed(
                            path,
                            from_entry @ ManifestEntry::Leaf(_),
                            to_entry @ ManifestEntry::Leaf(_),
                        ) => {
                            if !diff_files || !within_restrictions(&path, &path_restrictions) {
                                None
                            } else {
                                let (source, source_path, replacement_path) =
                                    Self::get_subtree_copy_source(
                                        &subtree_copy_sources,
                                        other,
                                        &path,
                                    )?;
                                let new_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        self.clone(),
                                        path.clone(),
                                        to_entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                let old_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        source,
                                        source_path,
                                        from_entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                if new_side.is_none() && old_side.is_none() {
                                    // Both sides denied: nothing visible.
                                    None
                                } else {
                                    // One denied side downgrades Changed to
                                    // Added (old denied) or Removed (new
                                    // denied); new_file takes Option sides.
                                    Some(ChangesetPathDiffContext::new_file(
                                        self.clone(),
                                        path,
                                        new_side,
                                        old_side,
                                        CopyInfo::None,
                                        replacement_path,
                                    )?)
                                }
                            }
                        }
                        ManifestDiff::Added(path, entry @ ManifestEntry::Tree(_)) => {
                            if !diff_trees || !within_restrictions(&path, &path_restrictions) {
                                None
                            } else {
                                let new_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        self.clone(),
                                        path.clone(),
                                        entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                match new_side {
                                    Some(new_ctx) => Some(ChangesetPathDiffContext::new_tree(
                                        self.clone(),
                                        path,
                                        Some(new_ctx),
                                        None,
                                        None,
                                    )?),
                                    None => None,
                                }
                            }
                        }
                        ManifestDiff::Removed(path, entry @ ManifestEntry::Tree(_)) => {
                            if !diff_trees || !within_restrictions(&path, &path_restrictions) {
                                None
                            } else {
                                let (source, source_path, replacement_path) =
                                    Self::get_subtree_copy_source(
                                        &subtree_copy_sources,
                                        other,
                                        &path,
                                    )?;
                                let old_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        source,
                                        source_path,
                                        entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                match old_side {
                                    Some(old_ctx) => Some(ChangesetPathDiffContext::new_tree(
                                        self.clone(),
                                        path,
                                        None,
                                        Some(old_ctx),
                                        replacement_path,
                                    )?),
                                    None => None,
                                }
                            }
                        }
                        ManifestDiff::Changed(
                            path,
                            from_entry @ ManifestEntry::Tree(_),
                            to_entry @ ManifestEntry::Tree(_),
                        ) => {
                            if !diff_trees || !within_restrictions(&path, &path_restrictions) {
                                None
                            } else {
                                let (source, source_path, replacement_path) =
                                    Self::get_subtree_copy_source(
                                        &subtree_copy_sources,
                                        other,
                                        &path,
                                    )?;
                                let new_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        self.clone(),
                                        path.clone(),
                                        to_entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                let old_side = policy_side(
                                    ChangesetPathContentContext::new_with_manifest_entry(
                                        source,
                                        source_path,
                                        from_entry,
                                    ),
                                    policy,
                                )
                                .await?;
                                if new_side.is_none() && old_side.is_none() {
                                    None
                                } else {
                                    Some(ChangesetPathDiffContext::new_tree(
                                        self.clone(),
                                        path,
                                        new_side,
                                        old_side,
                                        replacement_path,
                                    )?)
                                }
                            }
                        }
                        // We've already covered all practical possibilities as there are no "changed"
                        // between from trees and files as such are represented as removal+addition
                        _ => None,
                    };
                    Ok(entry)
                }
            })
            .take(limit.unwrap_or(usize::MAX))
            .try_collect::<Vec<_>>()
            .await?;

        let supplement_enabled = first_parent_is_other
            && diff_files
            && self
                .repo_ctx()
                .repo()
                .repo_config()
                .git_configs
                .git_lfs_interpret_pointers
            && justknobs::eval(
                ENABLE_BONSAI_ONLY_LFS_DIFFS,
                None,
                Some(self.repo_ctx().name()),
            );
        if supplement_enabled {
            // Ordered mode only needs supplement paths up to the final manifest
            // entry in this page: anything after it cannot affect the first
            // `limit` entries of the sorted union. If the manifest page is
            // short, scan to the end so supplement-only tail entries can fill
            // it.
            let supplement_before_or_at =
                if is_ordered && limit.is_some_and(|l| change_contexts.len() == l) {
                    change_contexts.last().map(|d| d.path().clone())
                } else {
                    None
                };
            let supplement_max = if is_ordered {
                limit.unwrap_or(usize::MAX)
            } else {
                limit.map_or(usize::MAX, |l| l.saturating_sub(change_contexts.len()))
            };

            if supplement_max > 0 {
                let excluded_paths: HashSet<MPath> =
                    change_contexts.iter().map(|c| c.path().clone()).collect();
                let supplement = self
                    .get_potential_lfs_changes(
                        other,
                        &path_restrictions,
                        &subtree_copy_sources,
                        &excluded_paths,
                        supplement_after.as_ref(),
                        supplement_before_or_at.as_ref(),
                        supplement_max,
                        policy,
                    )
                    .await?;
                insert_sorted_results(&mut change_contexts, supplement, is_ordered);
                if let Some(l) = limit {
                    change_contexts.truncate(l);
                }
            }
        }
        Ok(change_contexts)
    }

    pub async fn diff_root_unordered(
        &self,
        path_restrictions: Option<Vec<MPath>>,
        diff_items: BTreeSet<ChangesetDiffItem>,
        policy: &RestrictedPathsPolicy,
    ) -> Result<Vec<ChangesetPathDiffContext<R>>, MononokeError> {
        self.diff_root(
            path_restrictions,
            diff_items,
            ChangesetFileOrdering::Unordered,
            None,
            policy,
        )
        .await
    }

    /// Returns additions introduced by the root commit, a.k.a the initial commit
    ///
    /// `self` is considered the "root/initial/genesis" changeset
    /// `path_restrictions` if present will narrow down the diff to given paths
    /// `diff_items` what to include in the output (files, dirs or both)
    ///
    /// How restricted-path denials are handled is governed by `policy`:
    /// `Strict` fails the whole diff on any denial; `SkipAndCollect` omits
    /// denied entries, recording them against their denying restriction.
    /// Repo-auth denials and all other errors still fail.
    pub async fn diff_root(
        &self,
        path_restrictions: Option<Vec<MPath>>,
        diff_items: BTreeSet<ChangesetDiffItem>,
        ordering: ChangesetFileOrdering,
        limit: Option<usize>,
        policy: &RestrictedPathsPolicy,
    ) -> Result<Vec<ChangesetPathDiffContext<R>>, MononokeError> {
        let diff_files = diff_items.contains(&ChangesetDiffItem::FILES);
        let diff_trees = diff_items.contains(&ChangesetDiffItem::TREES);

        self.find_entries(to_vec1(path_restrictions), ordering)
            .watched()
            .await?
            .yield_periodically()
            .on_large_overshoot(|budget, elapsed| {
                warn!("yield_periodically(): budget overshot: current_budget={budget:?}, elapsed={elapsed:?}");
            })
            .try_filter_map(|(path, entry)| async move {
                match (path.into_optional_non_root_path(), entry) {
                    (Some(mpath), ManifestEntry::Leaf(_)) if diff_files => Ok(Some((MPath::from(mpath), false))),
                    (Some(mpath), ManifestEntry::Tree(_)) if diff_trees => Ok(Some((MPath::from(mpath), true))),
                    _ => Ok(None),
                }
            })
            .map_err(MononokeError::from)
            // Filter-then-limit: the page budget applies to visible entries,
            // so denied paths never consume it and an all-denied page still
            // advances past the walked entries.
            .try_filter_map(|(path, is_tree)| async move {
                let side = policy_side(
                    ChangesetPathContentContext::new(self.clone(), path.clone()),
                    policy,
                )
                .await?;
                Ok(side.map(|base| (base, path, is_tree)))
            })
            .take(limit.unwrap_or(usize::MAX))
            .and_then(|(base, path, is_tree)| async move {
                if is_tree {
                    Ok(ChangesetPathDiffContext::new_tree(
                        self.clone(),
                        path,
                        Some(base),
                        None,
                        None,
                    )?)
                } else {
                    Ok(ChangesetPathDiffContext::new_file(
                        self.clone(),
                        path,
                        Some(base),
                        None,
                        CopyInfo::None,
                        None,
                    )?)
                }
            })
            .try_collect::<Vec<_>>()
            .watched()
            .await
    }
}

impl<R: MononokeRepo> ChangesetContext<R> {
    pub async fn run_hooks(
        &self,
        bookmark: impl AsRef<str>,
        pushvars: Option<&HashMap<String, Bytes>>,
        run_as: Option<MononokeIdentitySet>,
        override_commit_message: Option<String>,
        log_only_rejections: LogOnlyRejections,
    ) -> Result<Vec<HookOutcome>, MononokeError> {
        // When `run_as` is provided, run the hooks against a context derived from
        // the caller's session with only the metadata identities swapped for the
        // `run_as` set (via `with_overridden_metadata`, which shares the rest of
        // the session -- rate limiters, session class, etc. -- and the caller's
        // logging container -- scuba, perf counters, scribe, sampling), so hooks
        // see them as the pusher while observability is otherwise unchanged. The
        // real caller's identities are captured for audit logging. Repository
        // access control has already been applied using the caller's real
        // identity when this `ChangesetContext` was created; `run_as` only
        // changes what the hooks observe. The request-level context
        // (`self.ctx()`) is left untouched, so logging after the hooks finish --
        // and at the end of the request -- still reflects the real caller.
        let run_as_original_identities = run_as
            .as_ref()
            .map(|_| self.ctx().metadata().identities().clone());
        let run_as_ctx = run_as.map(|identities| {
            let metadata = self.ctx().metadata().clone().set_identities(identities);
            self.ctx().with_overridden_metadata(Arc::new(metadata))
        });
        let ctx = run_as_ctx.as_ref().unwrap_or_else(|| self.ctx());
        // The override keeps the stored changeset id so hooks that look data
        // up by id (e.g. file lists) still resolve the real commit; only the
        // message hooks parse changes. The resulting value violates
        // id == hash(content) and exists solely for this in-memory dry run.
        let mut bonsai = self.bonsai_changeset().await?;
        if let Some(message) = override_commit_message {
            bonsai = bonsai.replace_message_keeping_id(message)?;
        }
        Ok(self
            .repo_ctx()
            .hook_manager()
            .run_changesets_hooks_for_bookmark(
                ctx,
                &[bonsai],
                &BookmarkKey::new(bookmark.as_ref())?,
                pushvars,
                CrossRepoPushSource::NativeToThisRepo,
                PushAuthoredBy::User,
                HookExecutionPurpose::DryRun,
                run_as_original_identities.as_ref(),
                log_only_rejections,
            )
            .await?)
    }

    /// Check commit rate limits for a changeset against a bookmark.
    pub async fn commit_rate_limit_check(
        &self,
        bookmark: &BookmarkKey,
    ) -> Result<CommitRateLimitCheckResult, MononokeError> {
        let bonsai = self.bonsai_changeset().await?;
        let result = check_all_commit_rate_limits(
            self.ctx(),
            self.repo_ctx().repo(),
            &bonsai,
            self.id(),
            bookmark,
        )
        .await
        .map_err(MononokeError::from)?;
        Ok(result)
    }

    pub async fn directory_branch_clusters(
        &self,
        filter_path_prefixes: Option<Vec<MPath>>,
        after: Option<MPath>,
    ) -> Result<impl Iterator<Item = DirectoryBranchCluster>, MononokeError> {
        // Cluster information is stored in the directory branch cluster
        // manifest, which can also be amended or overridden by configuration.
        let fixed_config = self
            .repo_ctx()
            .repo()
            .repo_config()
            .directory_branch_cluster_config
            .as_ref()
            .map(|config| &config.fixed_config);

        let root_skeleton_manifest_id = self
            .root_skeleton_manifest_id()
            .await?
            .into_skeleton_manifest_id();

        let dbcm_enabled = self
            .repo_ctx()
            .repo()
            .repo_derived_data()
            .config()
            .is_enabled(DerivableType::DirectoryBranchClusterManifest);
        let read_dbcm = dbcm_enabled
            && justknobs::eval(
                "scm/mononoke:dbcm_read_from_manifest",
                None,
                Some(self.repo_ctx().name()),
            );

        let mut clusters_by_primary: HashMap<MPath, DirectoryBranchCluster> = if read_dbcm {
            let root_dbcm = self
                .root_directory_branch_cluster_manifest_id()
                .await?
                .into_directory_branch_cluster_manifest_id()
                .load(self.ctx(), self.repo_ctx().repo().repo_blobstore())
                .await
                .map_err(MononokeError::from)?;

            let blobstore = self.repo_ctx().repo().repo_blobstore().clone();
            let ctx = self.ctx().clone();
            let manifest_clusters =
                collect_manifest_clusters(&ctx, &blobstore, root_dbcm, MPath::ROOT).await?;

            // Build a map of primary path -> cluster from manifest
            manifest_clusters
                .into_iter()
                .map(|c| (c.cluster_primary.clone(), c))
                .collect()
        } else {
            HashMap::new()
        };

        if let Some(fixed_config) = fixed_config {
            // Find which paths in the cluster config exist in this commit
            let config_paths: HashSet<MPath> = fixed_config
                .clusters
                .iter()
                .flat_map(|cluster| {
                    std::iter::once(&cluster.cluster_primary).chain(cluster.secondaries.iter())
                })
                .cloned()
                .map(MPath::from)
                .collect();
            let existing_paths = root_skeleton_manifest_id
                .find_entries(
                    self.ctx().clone(),
                    self.repo_ctx().repo().repo_blobstore().clone(),
                    config_paths.into_iter().map(PathOrPrefix::Path),
                )
                .map_ok(|(path, _entry)| path)
                .try_collect::<HashSet<_>>()
                .await?;

            let config_clusters: Vec<DirectoryBranchCluster> = fixed_config
                .clusters
                .iter()
                .cloned()
                .map(|mut cluster| {
                    cluster
                        .secondaries
                        .retain(|path| existing_paths.contains(<&MPath>::from(path)));
                    if !existing_paths.contains(<&MPath>::from(&cluster.cluster_primary))
                        && !cluster.secondaries.is_empty()
                    {
                        // If the primary path doesn't exist, promote the first secondary
                        cluster.cluster_primary = cluster.secondaries.remove(0);
                    }
                    cluster.into()
                })
                .filter(|cluster: &DirectoryBranchCluster| !cluster.secondaries.is_empty())
                .collect();

            // Merge config clusters with the manifest clusters.
            // If a cluster with the same primary exists in both config and
            // manifest, we take the union of the secondaries.
            // Note that we're assuming that if the config is attempting to
            // modify an existing cluster, it will have the same primary as
            // the manifest cluster - there's no attempt here to find 'overlapping'
            // clusters between the manifest and config that don't have the same
            // primary.
            for config_cluster in config_clusters.into_iter() {
                if let Some(manifest_cluster) =
                    clusters_by_primary.get_mut(&config_cluster.cluster_primary)
                {
                    // Union the secondaries from both sources
                    for secondary in config_cluster.secondaries {
                        if !manifest_cluster.secondaries.contains(&secondary) {
                            manifest_cluster.secondaries.push(secondary);
                        }
                    }
                } else {
                    clusters_by_primary
                        .insert(config_cluster.cluster_primary.clone(), config_cluster);
                }
            }
        }

        // Convert to a sorted vector, applying filters
        let mut clusters: Vec<_> = clusters_by_primary
            .into_values()
            .filter(|cluster| {
                // Apply after filter
                if let Some(after) = after.as_ref() {
                    if &cluster.cluster_primary <= after {
                        return false;
                    }
                }
                // Apply path prefix filter
                if let Some(filter_path_prefixes) = &filter_path_prefixes {
                    if !filter_path_prefixes.iter().any(|prefix| {
                        prefix.is_prefix_of(&cluster.cluster_primary)
                            || cluster.secondaries.iter().any(|s| prefix.is_prefix_of(s))
                    }) {
                        return false;
                    }
                }
                true
            })
            .collect();

        // Sort the clusters by primary path
        clusters.sort_by(|a, b| a.cluster_primary.cmp(&b.cluster_primary));

        Ok(clusters.into_iter())
    }

    /// Create a restriction context for querying restriction metadata about a path.
    ///
    /// Unlike `path()`, `path_with_content()`, and `path_with_history()`, this
    /// method does NOT perform Path ACLs access checks, since restriction queries are
    /// meta-queries about access policy. Repo read access is still required.
    pub async fn path_restriction(
        &self,
        path: impl Into<MPath>,
    ) -> Result<ChangesetPathRestrictionContext<R>, MononokeError> {
        ChangesetPathRestrictionContext::new(self.clone(), path.into()).await
    }

    /// Check restriction info for a batch of paths.
    ///
    /// For each path, returns the path and its restriction infos (empty if not restricted).
    /// A path under multiple nested roots will have multiple infos.
    /// Checks are performed concurrently.
    ///
    /// When `check_permissions` is true, each `PathAccessInfo` will have
    /// its `has_access` field populated with the result of an ACL check.
    /// When false, `has_access` will be `None`.
    pub async fn paths_restriction_info(
        &self,
        paths: Vec<NonRootMPath>,
        check_permissions: bool,
    ) -> Result<Vec<(NonRootMPath, Vec<PathAccessInfo>)>, MononokeError> {
        stream::iter(paths)
            .map(|path| async move {
                let restriction_ctx = self.path_restriction(path.clone()).await?;
                let infos = restriction_ctx.restriction_info(check_permissions).await?;
                Ok::<_, MononokeError>((path, infos))
            })
            .buffer_unordered(100)
            .try_collect()
            .await
    }

    /// Find restricted descendants under a set of root paths.
    ///
    /// Returns PathAccessInfo for all restriction roots
    /// that are descendants of any of the given root paths.
    /// Results are deduplicated by restriction_root.
    ///
    /// When `check_permissions` is true, each `PathAccessInfo` will have
    /// its `has_access` field populated with the result of an ACL check.
    /// When false, `has_access` will be `None`.
    pub async fn find_restricted_descendants(
        &self,
        roots: Vec<MPath>,
        check_permissions: bool,
    ) -> Result<Vec<PathAccessInfo>, MononokeError> {
        let restricted_paths = self.repo_ctx().repo().restricted_paths_arc();
        let cs_id = self.id();

        let restriction_infos = restricted_paths
            .find_restricted_descendants(self.ctx(), Some(cs_id), roots)
            .await?;

        if check_permissions {
            // TODO(T289977216): Route this through the Both-mode-aware checker
            // so tri-state factors are reconciled before producing has_access.
            let restriction_checks =
                check_path_restriction_infos(self.ctx(), &restricted_paths, restriction_infos)
                    .await?;
            return Ok(restriction_checks
                .into_iter()
                .map(|restriction_check| PathAccessInfo {
                    has_access: Some(restriction_check.has_authorization()),
                    restriction: restriction_check.into_restriction_info(),
                })
                .collect());
        }

        Ok(restriction_infos
            .into_iter()
            .map(|restriction| PathAccessInfo {
                restriction,
                has_access: None,
            })
            .collect())
    }

    /// Check if the repository may have restricted paths.
    pub fn may_have_restricted_paths(&self) -> bool {
        self.repo_ctx()
            .repo()
            .restricted_paths_arc()
            .may_have_restricted_paths()
    }
}
