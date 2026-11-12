/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg_attr(fbcode_build, feature(error_generic_member_access))]
#![feature(trait_alias)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Error;
use arc_swap::ArcSwap;
pub use bookmarks::BookmarkCategory;
pub use bookmarks::BookmarkKey;
use maplit::hashmap;
use mononoke_repos::MononokeRepos;
pub use mononoke_types::RepositoryId;
use repo_identity::RepoIdentityRef;
use tracing::Instrument;

use crate::repo::RepoContextBuilder;

pub mod changeset;
pub mod changeset_path;
pub mod changeset_path_diff;
pub mod errors;
pub mod file;
pub mod path;
pub mod repo;
pub mod restricted_paths;
pub mod sparse_profile;
pub mod specifiers;
pub mod tree;
mod xrepo;

#[cfg(test)]
mod test;

// Re-export types that are useful for clients.
pub use commit_rate_limit::CommitRateLimitCheckResult;
pub use commit_rate_limit::RateLimitOutcome;
pub use commit_rate_limit::RuleCheckResult;
pub use context::CoreContext;
pub use context::LoggingContainer;
pub use context::SessionContainer;
use metaconfig_types::CommitIdentityScheme;

pub use crate::changeset::ChangesetContext;
pub use crate::changeset::ChangesetDiffItem;
pub use crate::changeset::ChangesetFileOrdering;
pub use crate::changeset::ChangesetHistoryOptions;
pub use crate::changeset::ChangesetLinearHistoryOptions;
pub use crate::changeset::FingerprintVersion;
pub use crate::changeset::Generation;
pub use crate::changeset::PathVisibility;
pub use crate::changeset_path::ChangesetPathContentContext;
pub use crate::changeset_path::ChangesetPathHistoryOptions;
pub use crate::changeset_path::P4ChangesetRevisions;
pub use crate::changeset_path::P4LastChange;
pub use crate::changeset_path::P4PathRevision;
pub use crate::changeset_path::P4RevisionSearch;
pub use crate::changeset_path::PathEntry;
pub use crate::changeset_path_diff::ChangesetPathDiffContext;
pub use crate::changeset_path_diff::CopyInfo;
pub use crate::changeset_path_diff::FileContentType;
pub use crate::changeset_path_diff::FileGeneratedStatus;
pub use crate::changeset_path_diff::MetadataDiff;
pub use crate::changeset_path_diff::MetadataDiffFileInfo;
pub use crate::changeset_path_diff::MetadataDiffLinesCount;
pub use crate::changeset_path_diff::UnifiedDiff;
pub use crate::changeset_path_diff::UnifiedDiffMode;
pub use crate::errors::MononokeError;
pub use crate::file::FileContext;
pub use crate::file::FileId;
pub use crate::file::FileMetadata;
pub use crate::file::FileType;
pub use crate::file::HeaderlessUnifiedDiff;
pub use crate::file::headerless_unified_diff;
pub use crate::repo::BookmarkFreshness;
pub use crate::repo::BookmarkInfo;
pub use crate::repo::MononokeRepo;
pub use crate::repo::Repo;
pub use crate::repo::RepoContext;
pub use crate::repo::RepoWithBubble;
pub use crate::repo::StoreRequest;
pub use crate::repo::XRepoLookupExactBehaviour;
pub use crate::repo::XRepoLookupSyncBehaviour;
pub use crate::repo::create_changeset::CreateChange;
pub use crate::repo::create_changeset::CreateChangeFile;
pub use crate::repo::create_changeset::CreateChangeFileContents;
pub use crate::repo::create_changeset::CreateChangeGitLfs;
pub use crate::repo::create_changeset::CreateChangesetCheckMode;
pub use crate::repo::create_changeset::CreateChangesetChecks;
pub use crate::repo::create_changeset::CreateCopyInfo;
pub use crate::repo::create_changeset::CreateInfo;
pub use crate::repo::create_changeset::SNAPSHOT_BUBBLE_ID_EXTRA;
pub use crate::repo::land_stack::PushrebaseOutcome;
pub use crate::repo::update_submodule_expansion::SubmoduleExpansionUpdate;
pub use crate::repo::update_submodule_expansion::SubmoduleExpansionUpdateCommitInfo;
pub use crate::restricted_paths::ChangedPathRestrictionInfo;
pub use crate::restricted_paths::ChangedPathRestrictionSides;
pub use crate::restricted_paths::ChangedPathsRestrictionOptions;
pub use crate::restricted_paths::DenyingRestriction;
pub use crate::restricted_paths::OmissionCollector;
pub use crate::restricted_paths::PathAccessInfo;
pub use crate::restricted_paths::PathPermissionChangeInfo;
pub use crate::restricted_paths::PathPolicyInfo;
pub use crate::restricted_paths::PathRestrictionCopySourceInfo;
pub use crate::restricted_paths::RestrictedChangeGroup;
pub use crate::restricted_paths::RestrictedPathsChangesInfo;
pub use crate::restricted_paths::RestrictedPathsPolicy;
pub use crate::restricted_paths::SCS_ENABLE_PARTIAL_RESPONSES_JK;
pub use crate::restricted_paths::SubtreeSourceMapping;
pub use crate::restricted_paths::SubtreeSourcePolicyInfo;
pub use crate::specifiers::ChangesetId;
pub use crate::specifiers::ChangesetIdPrefix;
pub use crate::specifiers::ChangesetPrefixSpecifier;
pub use crate::specifiers::ChangesetSpecifier;
pub use crate::specifiers::ChangesetSpecifierPrefixResolution;
pub use crate::specifiers::Globalrev;
pub use crate::specifiers::HgChangesetId;
pub use crate::specifiers::HgChangesetIdPrefix;
pub use crate::tree::TreeContext;
pub use crate::xrepo::CandidateSelectionHintArgs;

/// An instance of Mononoke, which may manage multiple repositories.
pub struct Mononoke<R> {
    // Collection of instantiated repos currently being served by this task.
    // Deep-sharded: a given SCS task only holds the repos assigned to it by
    // shardmanager, not the full tier.
    pub repos: Arc<MononokeRepos<R>>,
    // Tier-wide list of all enabled repos (name -> default identity scheme),
    // sourced from configerator. Distinct from `repos` because `list_repos`
    // must return the tier-wide view, not this task's sharded subset.
    //
    // Wrapped in ArcSwap so MononokeConfigUpdateReceiver can refresh it on
    // every config change; reads from `list_repos` are lock-free via .load().
    pub repo_names_in_tier: Arc<ArcSwap<HashMap<String, CommitIdentityScheme>>>,
}

impl<R> Mononoke<R> {
    /// Create a MononokeAPI instance for MononokeRepos.
    ///
    /// `repo_names_in_tier` is shared with the owning MononokeReposManager and
    /// its MononokeConfigUpdateReceiver — updates land via the same ArcSwap.
    ///
    /// Note: a previous TODO suggested replacing this arg with `MononokeConfigs`
    /// so the tier list could be derived on demand. We deliberately keep the
    /// pre-computed ArcSwap: deriving from configs would iterate ~thousands of
    /// repos per `list_repos` call and would require plumbing `MononokeConfigs`
    /// through to here, which `Mononoke<R>` intentionally does not depend on.
    pub fn new(
        repos: Arc<MononokeRepos<R>>,
        repo_names_in_tier: Arc<ArcSwap<HashMap<String, CommitIdentityScheme>>>,
    ) -> Result<Self, Error> {
        Ok(Self {
            repos,
            repo_names_in_tier,
        })
    }

    /// Return the raw underlying repo corresponding to the provided
    /// repo name.
    pub fn raw_repo(&self, name: impl AsRef<str>) -> Option<Arc<R>> {
        self.repos.get_by_name(name.as_ref())
    }

    /// Return the raw underlying repo corresponding to the provided
    /// repo id.
    pub fn raw_repo_by_id(&self, id: i32) -> Option<Arc<R>> {
        self.repos.get_by_id(id)
    }

    /// Get all known repository ids
    pub fn known_repo_ids(&self) -> Vec<RepositoryId> {
        self.repos.iter_ids().map(RepositoryId::new).collect()
    }

    /// Returns an `Iterator` over all repo names.
    pub fn repo_names(&self) -> impl Iterator<Item = String> + use<R> {
        self.repos.iter_names()
    }

    pub fn repos(&self) -> impl Iterator<Item = Arc<R>> + use<R> {
        self.repos.iter()
    }
}

impl<R: RepoIdentityRef> Mononoke<R> {
    pub fn repo_name_from_id(&self, repo_id: RepositoryId) -> Option<String> {
        self.repos
            .get_by_id(repo_id.id())
            .map(|repo| repo.repo_identity().name().to_string())
    }

    pub fn repo_id_from_name(&self, name: impl AsRef<str>) -> Option<RepositoryId> {
        self.repos
            .get_by_name(name.as_ref())
            .map(|repo| repo.repo_identity().id())
    }
}

impl<R> Mononoke<R> {
    /// Start a request on a repository by name.
    // Async and fallible because this can instantiate the repo lazily. The
    // bound is the detached build's, not this method's: it sits here rather
    // than on the impl block so nothing that never builds a repo inherits it.
    pub async fn repo(
        &self,
        ctx: CoreContext,
        name: impl AsRef<str>,
    ) -> Result<Option<RepoContextBuilder<R>>, MononokeError>
    where
        R: Send + Sync + 'static,
    {
        match self.repos.get(name.as_ref()).await? {
            None => Ok(None),
            Some(repo) => Ok(Some(
                RepoContextBuilder::new(ctx, repo, self.repos.clone()).await?,
            )),
        }
    }

    /// Start a request on a repository by id.
    // Method is async and fallible as in the future this may involve
    // instantiating the repo lazily.
    pub async fn repo_by_id(
        &self,
        ctx: CoreContext,
        repo_id: RepositoryId,
    ) -> Result<Option<RepoContextBuilder<R>>, MononokeError> {
        match self.repos.get_by_id(repo_id.id()) {
            None => Ok(None),
            Some(repo) => Ok(Some(
                RepoContextBuilder::new(ctx, repo, self.repos.clone()).await?,
            )),
        }
    }
}

impl<R: MononokeRepo> Mononoke<R> {
    /// Report configured monitoring stats
    pub async fn report_monitoring_stats(&self, ctx: &CoreContext) -> Result<(), MononokeError> {
        for repo in self.repos.iter() {
            let span =
                tracing::info_span!("bookmark monitoring", repo = %repo.repo_identity().name());
            crate::repo::report_monitoring_stats(ctx, &repo)
                .instrument(span)
                .await?;
        }

        Ok(())
    }
}

pub(crate) fn invalid_push_redirected_request(method_name: &str) -> MononokeError {
    MononokeError::InvalidRequest(format!(
        "{method_name} is not supported for push redirected repos"
    ))
}

pub mod test_impl {
    use repo_identity::RepoIdentityRef;

    use super::*;

    impl Mononoke<Repo> {
        /// Create a Mononoke instance for testing.
        pub async fn new_test(
            repos: impl IntoIterator<Item = (String, Repo)>,
        ) -> Result<Self, Error> {
            let repos = repos
                .into_iter()
                .map(|(name, repo)| (repo.repo_identity().id().id(), name, repo))
                .collect::<Vec<_>>();
            let repo_names_in_tier = repos
                .iter()
                .map(|(_, name, repo)| {
                    (
                        name.to_string(),
                        repo.repo_config.default_commit_identity_scheme.clone(),
                    )
                })
                .collect::<HashMap<_, _>>();
            let mononoke_repos = MononokeRepos::new();
            mononoke_repos.populate(repos);
            Ok(Self {
                repos: Arc::new(mononoke_repos),
                repo_names_in_tier: Arc::new(ArcSwap::from_pointee(repo_names_in_tier)),
            })
        }

        pub async fn new_test_xrepo(small_repo: Repo, large_repo: Repo) -> Result<Self, Error> {
            let repo_names_in_tier = hashmap! {
                small_repo.repo_identity().name().to_string() => CommitIdentityScheme::GIT,
                large_repo.repo_identity().name().to_string() => CommitIdentityScheme::HG,
            };
            let mononoke_repos = MononokeRepos::new();
            mononoke_repos.populate(vec![
                (
                    small_repo.repo_identity().id().id(),
                    small_repo.repo_identity().name().to_string(),
                    small_repo,
                ),
                (
                    large_repo.repo_identity().id().id(),
                    large_repo.repo_identity().name().to_string(),
                    large_repo,
                ),
            ]);
            Ok(Self {
                repos: Arc::new(mononoke_repos),
                repo_names_in_tier: Arc::new(ArcSwap::from_pointee(repo_names_in_tier)),
            })
        }
    }
}
