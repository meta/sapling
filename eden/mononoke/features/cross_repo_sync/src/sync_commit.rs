/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod in_memory;

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use anyhow::format_err;
use blobstore::Loadable;
use bookmark_renaming::BookmarkRenamer;
use bookmarks::BookmarkKey;
use cacheblob::LeaseOps;
use commit_transformation::SubmoduleDeps;
use commit_transformation::git_submodules::InMemoryRepo;
use commit_transformation::git_submodules::SubmoduleExpansionData;
use commit_transformation::rewrite_commit;
use commit_transformation::upload_commits;
use context::CoreContext;
use futures::FutureExt;
use futures::future;
use futures::future::TryFutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use in_memory::CommitInMemorySyncer;
use in_memory::unsafe_sync_commit_in_memory;
use live_commit_sync_config::LiveCommitSyncConfig;
use maplit::hashmap;
use maplit::hashset;
use metaconfig_types::CommitSyncConfigVersion;
use metaconfig_types::CommitSyncDirection;
use metaconfig_types::PushrebaseFlags;
use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::RepositoryId;
use movers::Mover;
use movers::Movers;
use pushrebase::do_pushrebase_bonsai;
use pushrebase_hooks::get_pushrebase_hooks;
use repo_blobstore::RepoBlobstoreRef;
use reporting::CommitSyncContext;
use reporting::log_rewrite;
use reporting::set_scuba_logger_fields;
use scuba_ext::MononokeScubaSampleBuilder;
use synced_commit_mapping::ArcSyncedCommitMapping;
use synced_commit_mapping::EquivalentWorkingCopyEntry;
use synced_commit_mapping::SyncedCommitMapping;
use synced_commit_mapping::SyncedCommitSourceRepo;
use synced_commit_mapping_pushrebase_hook::ForwardSyncedCommitInfo;
use tracing::debug;

use crate::commit_sync_config_utils::get_bookmark_renamer;
use crate::commit_sync_config_utils::get_common_pushrebase_bookmarks;
use crate::commit_sync_config_utils::get_git_submodule_action_by_version;
use crate::commit_sync_config_utils::get_reverse_bookmark_renamer;
use crate::commit_sync_config_utils::get_reverse_mover;
use crate::commit_sync_config_utils::version_exists;
use crate::commit_sync_outcome::CandidateSelectionHint;
use crate::commit_sync_outcome::CommitSyncOutcome;
use crate::commit_sync_outcome::PluralCommitSyncOutcome;
use crate::commit_sync_outcome::commit_sync_outcome_exists;
use crate::commit_sync_outcome::get_commit_sync_outcome;
use crate::commit_sync_outcome::get_commit_sync_outcome_with_hint;
use crate::commit_sync_outcome::get_plural_commit_sync_outcome;
use crate::commit_syncers_lib::CommitSyncRepos;
use crate::commit_syncers_lib::SyncedAncestorsVersions;
use crate::commit_syncers_lib::find_toposorted_unsynced_ancestors;
use crate::commit_syncers_lib::get_movers_by_version;
use crate::commit_syncers_lib::remap_parents;
use crate::commit_syncers_lib::run_with_lease;
use crate::commit_syncers_lib::submodule_metadata_file_prefix_and_dangling_pointers;
use crate::commit_syncers_lib::submodule_repos_with_content_ids;
use crate::commit_syncers_lib::update_mapping_with_version;
use crate::sync_config_version_utils::get_version;
use crate::sync_config_version_utils::set_mapping_change_version;
use crate::types::CrossRepoSyncError;
use crate::types::PushrebaseRewriteDates;
use crate::types::Repo;
use crate::types::Source;
use crate::types::Target;

#[derive(Clone)]
pub struct CommitSyncData<R> {
    pub repos: CommitSyncRepos<R>,
    pub live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    pub scuba_sample: MononokeScubaSampleBuilder,
}

impl<R> fmt::Debug for CommitSyncData<R>
where
    R: Repo,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source_repo_id = self.get_source_repo_id();
        let target_repo_id = self.get_target_repo_id();
        write!(f, "CommitSyncData{{{source_repo_id}->{target_repo_id}}}")
    }
}

impl<R> CommitSyncData<R>
where
    R: Repo,
{
    // ------------------------------------------------------------------------
    // Constructors

    pub fn new(
        ctx: &CoreContext,
        repos: CommitSyncRepos<R>,
        live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    ) -> Self {
        let scuba_sample = reporting::get_scuba_sample(
            ctx,
            repos.get_source_repo().repo_identity().name(),
            repos.get_target_repo().repo_identity().name(),
        );

        Self {
            repos,
            live_commit_sync_config,
            scuba_sample,
        }
    }

    // Builds the syncer that can be used for opposite sync direction.
    // Note: doesn't support large-to-small as input right now
    pub fn reverse(&self) -> CommitSyncData<R> {
        Self {
            repos: self.repos.reverse(),
            live_commit_sync_config: self.live_commit_sync_config.clone(),
            scuba_sample: self.scuba_sample.clone(),
        }
    }

    // ------------------------------------------------------------------------
    // Getters

    pub fn get_source_repo(&self) -> &R {
        self.repos.get_source_repo()
    }

    pub fn get_submodule_deps(&self) -> &SubmoduleDeps<R> {
        self.repos.get_submodule_deps()
    }

    pub fn get_source_repo_id(&self) -> RepositoryId {
        self.get_source_repo().repo_identity().id()
    }

    pub fn get_target_repo(&self) -> &R {
        self.repos.get_target_repo()
    }

    pub fn get_target_repo_id(&self) -> RepositoryId {
        self.get_target_repo().repo_identity().id()
    }

    pub fn get_source_repo_type(&self) -> SyncedCommitSourceRepo {
        self.repos.get_source_repo_type()
    }

    pub fn get_large_repo(&self) -> &R {
        self.repos.get_large_repo()
    }

    pub fn get_small_repo(&self) -> &R {
        self.repos.get_small_repo()
    }

    pub fn get_mapping(&self) -> &ArcSyncedCommitMapping {
        self.repos.get_mapping()
    }

    pub fn get_x_repo_sync_lease(&self) -> &Arc<dyn LeaseOps> {
        self.repos.get_x_repo_sync_lease()
    }

    pub fn get_live_commit_sync_config(&self) -> &Arc<dyn LiveCommitSyncConfig> {
        &self.live_commit_sync_config
    }

    // -- Getters
    // ------------------------------------------------------------------------

    // TODO(T182311609): unify commit sync outcome methods
    pub async fn get_plural_commit_sync_outcome<'a>(
        &'a self,
        ctx: &'a CoreContext,
        source_cs_id: ChangesetId,
    ) -> Result<Option<PluralCommitSyncOutcome>, Error> {
        get_plural_commit_sync_outcome(
            ctx,
            Source(self.repos.get_source_repo().repo_identity().id()),
            Target(self.repos.get_target_repo().repo_identity().id()),
            Source(source_cs_id),
            self.get_mapping(),
            self.repos.get_direction(),
            Arc::clone(&self.live_commit_sync_config),
        )
        .boxed()
        .await
    }

    pub async fn get_commit_sync_outcome<'a>(
        &'a self,
        ctx: &'a CoreContext,
        source_cs_id: ChangesetId,
    ) -> Result<Option<CommitSyncOutcome>, Error> {
        get_commit_sync_outcome(
            ctx,
            Source(self.repos.get_source_repo().repo_identity().id()),
            Target(self.repos.get_target_repo().repo_identity().id()),
            Source(source_cs_id),
            self.get_mapping(),
            self.repos.get_direction(),
            Arc::clone(&self.live_commit_sync_config),
        )
        .boxed()
        .await
    }

    pub async fn get_commit_sync_outcome_with_hint<'a>(
        &'a self,
        ctx: &'a CoreContext,
        source_cs_id: Source<ChangesetId>,
        hint: CandidateSelectionHint<R>,
    ) -> Result<Option<CommitSyncOutcome>, Error> {
        get_commit_sync_outcome_with_hint(
            ctx,
            Source(self.repos.get_source_repo().repo_identity().id()),
            Target(self.repos.get_target_repo().repo_identity().id()),
            source_cs_id,
            self.get_mapping(),
            hint,
            self.repos.get_direction(),
            Arc::clone(&self.live_commit_sync_config),
        )
        .boxed()
        .await
    }

    // TODO(T182311609): should this be public?
    pub async fn get_common_pushrebase_bookmarks(&self) -> Result<Vec<BookmarkKey>, Error> {
        get_common_pushrebase_bookmarks(
            Arc::clone(&self.live_commit_sync_config),
            self.get_small_repo().repo_identity().id(),
        )
        .boxed()
        .await
    }

    // TODO(T182311609): delete this. It shouldn't be used by CommitSyncData clients.
    pub async fn get_movers_by_version(
        &self,
        version: &CommitSyncConfigVersion,
    ) -> Result<Movers, Error> {
        get_movers_by_version(
            version,
            Arc::clone(&self.live_commit_sync_config),
            Source(self.repos.get_source_repo().repo_identity().id()),
            Target(self.repos.get_target_repo().repo_identity().id()),
        )
        .boxed()
        .await
    }

    // TODO(T182311609): delete this. It shouldn't be used by CommitSyncData clients.
    pub async fn get_reverse_mover_by_version(
        &self,
        version: &CommitSyncConfigVersion,
    ) -> Result<Arc<dyn Mover>, Error> {
        let (source_repo, target_repo) = self.get_source_target();
        get_reverse_mover(
            Arc::clone(&self.live_commit_sync_config),
            version,
            source_repo.repo_identity().id(),
            target_repo.repo_identity().id(),
        )
        .boxed()
        .await
    }

    // TODO(T182311609): delete this. It shouldn't be used by CommitSyncData clients.
    pub(crate) async fn get_reverse_bookmark_renamer(&self) -> Result<BookmarkRenamer, Error> {
        let (source_repo, target_repo) = self.get_source_target();

        get_reverse_bookmark_renamer(
            Arc::clone(&self.live_commit_sync_config),
            source_repo.repo_identity().id(),
            target_repo.repo_identity().id(),
        )
        .boxed()
        .await
    }

    // ------------------------------------------------------------------------
    // Other methods

    pub async fn version_exists(&self, version: &CommitSyncConfigVersion) -> Result<bool, Error> {
        version_exists(
            Arc::clone(&self.live_commit_sync_config),
            self.get_target_repo_id(),
            version,
        )
        .boxed()
        .await
    }

    pub async fn rename_bookmark(
        &self,
        bookmark: &BookmarkKey,
    ) -> Result<Option<BookmarkKey>, Error> {
        Ok(self.get_bookmark_renamer().boxed().await?(bookmark))
    }

    pub async fn commit_sync_outcome_exists<'a>(
        &'a self,
        ctx: &'a CoreContext,
        source_cs_id: Source<ChangesetId>,
    ) -> Result<bool, Error> {
        commit_sync_outcome_exists(
            ctx,
            Source(self.repos.get_source_repo().repo_identity().id()),
            Target(self.repos.get_target_repo().repo_identity().id()),
            source_cs_id,
            self.get_mapping(),
            self.repos.get_direction(),
            Arc::clone(&self.live_commit_sync_config),
        )
        .boxed()
        .await
    }

    // Rewrites a commit and uploads it
    pub(crate) async fn upload_rewritten_and_update_mapping<'a, RB: RepoBlobstoreRef>(
        &'a self,
        ctx: &'a CoreContext,
        source_cs_id: ChangesetId,
        rewritten: BonsaiChangesetMut,
        submodule_content_ids: Vec<(Arc<RB>, HashSet<ContentId>)>,
        version: CommitSyncConfigVersion,
    ) -> Result<ChangesetId, Error> {
        let (source_repo, target_repo) = self.get_source_target();

        let frozen = rewritten.freeze()?;
        let target_cs_id = frozen.get_changeset_id();
        upload_commits(
            ctx,
            vec![frozen],
            &source_repo,
            &target_repo,
            submodule_content_ids,
        )
        .await?;

        // update_mapping also updates working copy equivalence, so no need
        // to do it separately
        update_mapping_with_version(
            ctx,
            hashmap! { source_cs_id =>  target_cs_id},
            self,
            &version,
        )
        .await?;
        Ok(target_cs_id)
    }

    pub(crate) async fn set_no_sync_candidate<'a>(
        &'a self,
        ctx: &'a CoreContext,
        source_bcs_id: ChangesetId,
        version_name: CommitSyncConfigVersion,
    ) -> Result<(), Error> {
        self.update_wc_equivalence_with_version(ctx, source_bcs_id, None, version_name)
            .await
    }

    pub(crate) async fn update_wc_equivalence_with_version<'a>(
        &'a self,
        ctx: &'a CoreContext,
        source_bcs_id: ChangesetId,
        maybe_target_bcs_id: Option<ChangesetId>,
        version_name: CommitSyncConfigVersion,
    ) -> Result<(), Error> {
        let xrepo_sync_disable_all_syncs =
            justknobs::eval("scm/mononoke:xrepo_sync_disable_all_syncs", None, None);
        if xrepo_sync_disable_all_syncs {
            return Err(CrossRepoSyncError::XRepoSyncDisabled.into());
        }

        let small_repo = self.get_small_repo();
        let large_repo = self.get_large_repo();
        let (source_repo, target_repo, source_is_large) = match self.repos.get_direction() {
            CommitSyncDirection::Backwards => (large_repo.clone(), small_repo.clone(), true),
            CommitSyncDirection::Forward => (small_repo.clone(), large_repo.clone(), true),
        };

        let source_repoid = source_repo.repo_identity().id();
        let target_repoid = target_repo.repo_identity().id();

        let wc_entry = match maybe_target_bcs_id {
            Some(target_bcs_id) => {
                if source_is_large {
                    EquivalentWorkingCopyEntry {
                        large_repo_id: source_repoid,
                        large_bcs_id: source_bcs_id,
                        small_repo_id: target_repoid,
                        small_bcs_id: Some(target_bcs_id),
                        version_name: Some(version_name),
                    }
                } else {
                    EquivalentWorkingCopyEntry {
                        large_repo_id: target_repoid,
                        large_bcs_id: target_bcs_id,
                        small_repo_id: source_repoid,
                        small_bcs_id: Some(source_bcs_id),
                        version_name: Some(version_name),
                    }
                }
            }
            None => {
                if !source_is_large {
                    bail!(
                        "unexpected wc equivalence update: small repo commit should always remap to large repo"
                    );
                }
                EquivalentWorkingCopyEntry {
                    large_repo_id: source_repoid,
                    large_bcs_id: source_bcs_id,
                    small_repo_id: target_repoid,
                    small_bcs_id: None,
                    version_name: Some(version_name),
                }
            }
        };

        self.get_mapping()
            .insert_equivalent_working_copy(ctx, wc_entry)
            .await
            .map(|_| ())
    }

    // -------------------------------------------------------------------------
    // Private methods

    // Get a version to use while syncing ancestor with no parent  of `source_cs_id`
    // We only allow syncing such commits if we an unambiguously decide on the CommitSyncConfig version to use,
    // and we do that by ensuring that there is exactly one unique version among the commit sync outcomes
    // of all the already-synced ancestors of `source_cs_id`
    async fn get_version_for_syncing_commit_with_no_parent(
        &self,
        ctx: &CoreContext,
        commit_with_no_parent: ChangesetId,
        synced_ancestors_versions: &SyncedAncestorsVersions,
    ) -> Result<CommitSyncConfigVersion, Error> {
        let maybe_version =
            get_version(ctx, self.get_source_repo(), commit_with_no_parent, vec![]).await?;
        let version = match maybe_version {
            Some(version) => version,
            None => synced_ancestors_versions
                .get_only_version()?
                .ok_or_else(|| format_err!("no versions found for {commit_with_no_parent}"))?,
        };
        Ok(version)
    }

    fn get_source_target(&self) -> (R, R) {
        let small_repo = self.repos.get_small_repo().clone();
        let large_repo = self.repos.get_large_repo().clone();
        match self.repos.get_direction() {
            CommitSyncDirection::Backwards => (large_repo, small_repo),
            CommitSyncDirection::Forward => (small_repo, large_repo),
        }
    }

    async fn get_bookmark_renamer(&self) -> Result<BookmarkRenamer, Error> {
        let (source_repo, target_repo) = self.get_source_target();

        get_bookmark_renamer(
            Arc::clone(&self.live_commit_sync_config),
            source_repo.repo_identity().id(),
            target_repo.repo_identity().id(),
        )
        .boxed()
        .await
    }
}

// ------------------------------------------------------------------------
// Sync functions

/// This is the function that safely syncs a commit and all of its unsynced ancestors from a
/// source repo to target repo. If commit is already synced then it just does a lookup.
/// But safety comes with flexibility cost - not all of the syncs are allowed. For example,
/// syncing a *public* commit from a small repo to a large repo is not allowed:
/// 1) If small repo is the source of truth, then there should be only a single job that
///    does this sync. Since this function can be used from many places and we have no
///    way of ensuring only a single job does the sync, this sync is forbidden completely.
/// 2) If large repo is a source of truth, then there should never be a case with public
///    commit in a small repo not having an equivalent in the large repo.
pub async fn sync_commit<R: Repo>(
    ctx: &CoreContext,
    source_cs_id: ChangesetId,
    commit_sync_data: &CommitSyncData<R>,
    ancestor_selection_hint: CandidateSelectionHint<R>,
    commit_sync_context: CommitSyncContext,
    disable_lease: bool,
) -> Result<Option<ChangesetId>, Error> {
    let before = Instant::now();
    let res = sync_commit_impl(
        ctx,
        commit_sync_data,
        source_cs_id,
        commit_sync_context,
        ancestor_selection_hint,
        disable_lease,
    )
    .boxed()
    .await;
    let elapsed = before.elapsed();
    log_rewrite(
        ctx,
        commit_sync_data.scuba_sample.clone(),
        source_cs_id,
        "sync_commit",
        commit_sync_context,
        elapsed,
        &res,
    );
    res
}

/// Create a changeset, equivalent to `source_cs_id` in the target repo
/// The difference between this function and `rewrite_commit` is that
/// `rewrite_commit` does not know anything about the repo and only produces
/// a `BonsaiChangesetMut` object, which later may or may not be uploaded
/// into the repository.
/// This function is prefixed with unsafe because it requires that ancestors commits are
/// already synced and because syncing commit public commits from a small repo to a large repo
/// using this function might lead to repo corruption.
/// `parent_selection_hint` is used when remapping this commit's parents.
/// See `CandidateSelectionHint` doctring for more details.
pub async fn unsafe_sync_commit<R: Repo>(
    ctx: &CoreContext,
    source_cs_id: ChangesetId,
    commit_sync_data: &CommitSyncData<R>,
    parent_mapping_selection_hint: CandidateSelectionHint<R>,
    commit_sync_context: CommitSyncContext,
    // For commits that have at least a single parent it checks that these commits
    // will be rewritten with this version, and for commits with no parents
    // this expected version will be used for rewriting.
    expected_version: Option<CommitSyncConfigVersion>,
    // TODO(T182311609): extract options into a struct with reasonable defaults.
    // Add the name of the commit sync mapping version used to sync the commit
    // to the rewritten commit's hg extra.
    add_mapping_to_hg_extra: bool,
) -> Result<Option<ChangesetId>, Error> {
    let before = Instant::now();
    let res = unsafe_sync_commit_impl(
        ctx,
        source_cs_id,
        commit_sync_data,
        parent_mapping_selection_hint,
        commit_sync_context,
        expected_version,
        add_mapping_to_hg_extra,
    )
    .boxed()
    .await;
    let elapsed = before.elapsed();
    log_rewrite(
        ctx,
        commit_sync_data.scuba_sample.clone(),
        source_cs_id,
        "unsafe_sync_commit",
        commit_sync_context,
        elapsed,
        &res,
    );
    res
}

/// Rewrite a commit and creates in target repo if parents are already created.
/// This is marked as unsafe since it might lead to repo corruption if used incorrectly.
/// It can be used to import a merge commit from a new repo:
///
/// ```text
///     source repo:
///
///     O  <- master (common bookmark). Points to a merge commit that imports a new repo
///     | \
///     O   \
///          O  <- merge commit in the new repo we are trying to merge into master.
///         /  \   naive_sync_commit can be used to sync this commit
/// ```
///
/// Normally this function is able to find the parents for the synced commit automatically
/// but in case it can't then `maybe_parents` parameter allows us to overwrite parents of
/// the synced commit.
pub async fn unsafe_always_rewrite_sync_commit<'a, R: Repo>(
    ctx: &'a CoreContext,
    source_cs_id: ChangesetId,
    commit_sync_data: &CommitSyncData<R>,
    maybe_parents: Option<HashMap<ChangesetId, ChangesetId>>,
    sync_config_version: &CommitSyncConfigVersion,
    commit_sync_context: CommitSyncContext,
) -> Result<Option<ChangesetId>, Error> {
    let before = Instant::now();
    let res = unsafe_always_rewrite_sync_commit_impl(
        ctx,
        source_cs_id,
        commit_sync_data,
        maybe_parents,
        sync_config_version,
    )
    .boxed()
    .await;
    let elapsed = before.elapsed();
    log_rewrite(
        ctx,
        commit_sync_data.scuba_sample.clone(),
        source_cs_id,
        "unsafe_always_rewrite_sync_commit",
        commit_sync_context,
        elapsed,
        &res,
    );
    res
}

/// This function is prefixed with unsafe because it requires that ancestors commits are
/// already synced and because there should be exactly one sync job that uses this function
/// for a (small repo -> large repo) pair.
///
/// Validation that the version is applicable is done by the caller.
///
/// Optional parent mapping allows to override the selection for the parent when rewriting small
/// repo commit to large repo before pushrebasing it. Such commit must be one of the commits with
/// working copy equivalent to the small repo commit.
pub async fn unsafe_sync_commit_pushrebase<'a, R: Repo>(
    ctx: &'a CoreContext,
    source_cs: BonsaiChangeset,
    commit_sync_data: &'a CommitSyncData<R>,
    target_bookmark: Target<BookmarkKey>,
    commit_sync_context: CommitSyncContext,
    rewritedates: PushrebaseRewriteDates,
    version: CommitSyncConfigVersion,
    change_mapping_version: Option<CommitSyncConfigVersion>,
    parent_mapping: HashMap<ChangesetId, ChangesetId>,
) -> Result<Option<ChangesetId>, Error> {
    let source_cs_id = source_cs.get_changeset_id();
    let before = Instant::now();
    let res = unsafe_sync_commit_pushrebase_impl(
        ctx,
        source_cs,
        commit_sync_data,
        target_bookmark,
        rewritedates,
        version,
        change_mapping_version,
        parent_mapping,
    )
    .boxed()
    .await;
    let elapsed = before.elapsed();

    log_rewrite(
        ctx,
        commit_sync_data.scuba_sample.clone(),
        source_cs_id,
        "unsafe_sync_commit_pushrebase",
        commit_sync_context,
        elapsed,
        &res,
    );
    res
}

// -------------------------------------------------------------------------
// Private functions

async fn sync_commit_impl<R: Repo>(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<R>,
    source_cs_id: ChangesetId,
    commit_sync_context: CommitSyncContext,
    ancestor_selection_hint: CandidateSelectionHint<R>,
    disable_lease: bool,
) -> Result<Option<ChangesetId>, Error> {
    let (unsynced_ancestors, synced_ancestors_versions) =
        find_toposorted_unsynced_ancestors(ctx, commit_sync_data, source_cs_id, None).await?;

    let source_repo = commit_sync_data.repos.get_source_repo();
    let target_repo = commit_sync_data.repos.get_target_repo();

    let small_repo = commit_sync_data.get_small_repo();
    let source_repo_is_small = source_repo.repo_identity().id() == small_repo.repo_identity().id();

    if source_repo_is_small {
        let public_unsynced_ancestors = source_repo
            .phases()
            .get_public(
                ctx,
                unsynced_ancestors.clone(),
                false, /* ephemeral_derive */
            )
            .await?;
        if !public_unsynced_ancestors.is_empty() {
            return Err(format_err!(
                "unexpected sync lookup attempt - trying to sync \
                   a public commit from small repo to a large repo. Syncing public commits is \
                   only supported from a large repo to a small repo"
            ));
        }
    }

    for ancestor in unsynced_ancestors {
        let lease_key = format!(
            "sourcerepo_{}_targetrepo_{}.{}",
            source_repo.repo_identity().id().id(),
            target_repo.repo_identity().id().id(),
            source_cs_id,
        );

        let checker = || async {
            let maybe_outcome = commit_sync_data
                .get_commit_sync_outcome(ctx, source_cs_id)
                .await?;
            Result::<_, Error>::Ok(maybe_outcome.is_some())
        };
        let sync = || async {
            let parents = commit_sync_data
                .get_source_repo()
                .commit_graph()
                .changeset_parents(ctx, ancestor)
                .await?;
            let expected_version = if parents.is_empty() {
                let version = commit_sync_data
                    .get_version_for_syncing_commit_with_no_parent(
                        ctx,
                        ancestor,
                        &synced_ancestors_versions,
                    )
                    .await
                    .with_context(|| {
                        format_err!("failed to sync ancestor {ancestor} of {source_cs_id}")
                    })?;

                Some(version)
            } else {
                None
            };

            unsafe_sync_commit_impl(
                ctx,
                ancestor,
                commit_sync_data,
                ancestor_selection_hint.clone(),
                commit_sync_context,
                expected_version,
                false, // add_mapping_to_hg_extra
            )
            .await?;
            Ok(())
        };
        if disable_lease {
            sync().await?;
        } else {
            run_with_lease(
                ctx,
                commit_sync_data.get_x_repo_sync_lease(),
                lease_key,
                checker,
                sync,
            )
            .await?;
        }
    }

    let commit_sync_outcome = commit_sync_data
        .get_commit_sync_outcome(ctx, source_cs_id)
        .await?
        .ok_or_else(|| format_err!("was not able to remap a commit {source_cs_id}"))?;
    use CommitSyncOutcome::*;
    let res = match commit_sync_outcome {
        NotSyncCandidate(_) => None,
        RewrittenAs(cs_id, _) | EquivalentWorkingCopyAncestor(cs_id, _) => Some(cs_id),
    };
    Ok(res)
}

async fn unsafe_sync_commit_impl<'a, R: Repo>(
    ctx: &'a CoreContext,
    source_cs_id: ChangesetId,
    commit_sync_data: &'a CommitSyncData<R>,
    mut parent_mapping_selection_hint: CandidateSelectionHint<R>,
    commit_sync_context: CommitSyncContext,
    expected_version: Option<CommitSyncConfigVersion>,
    add_mapping_to_hg_extra: bool,
) -> Result<Option<ChangesetId>, Error> {
    let ctx = &set_scuba_logger_fields(ctx, [("sync_context", commit_sync_context.to_string())]);
    debug!(
        "{:?}: unsafe_sync_commit called for {}, with hint: {:?}",
        commit_sync_data, source_cs_id, parent_mapping_selection_hint
    );
    let source_repo = commit_sync_data.get_source_repo();
    let cs = source_cs_id.load(ctx, source_repo.repo_blobstore()).await?;
    if cs.parents().count() > 1 {
        parent_mapping_selection_hint = CandidateSelectionHint::Only;
    }
    let mapped_parents = stream::iter(cs.parents().map(|p| {
        commit_sync_data
            .get_commit_sync_outcome_with_hint(
                ctx,
                Source(p),
                parent_mapping_selection_hint.clone(),
            )
            .and_then(move |maybe_outcome| match maybe_outcome {
                Some(outcome) => future::ok((p, outcome)),
                None => future::err(format_err!("{p} does not have CommitSyncOutcome")),
            })
    }))
    .buffered(100)
    .try_collect()
    .await?;

    let submodule_deps = commit_sync_data.get_submodule_deps();
    let fallback_repos = vec![Arc::new(source_repo.clone())]
        .into_iter()
        .chain(submodule_deps.repos())
        .collect::<Vec<_>>();
    let large_repo = commit_sync_data.get_large_repo();
    let large_in_memory_repo = InMemoryRepo::from_repo(large_repo, fallback_repos)?;
    let strip_commit_extras = commit_sync_data.repos.get_strip_commit_extras()?;
    let should_set_committer_info_to_author_info_if_empty = commit_sync_data
        .repos
        .should_set_committer_info_to_author_info_if_empty()?;

    let in_memory_syncer = CommitInMemorySyncer {
        ctx,
        source_repo: Source(commit_sync_data.get_source_repo()),
        mapped_parents: &mapped_parents,
        target_repo_id: Target(commit_sync_data.get_target_repo_id()),
        live_commit_sync_config: Arc::clone(&commit_sync_data.live_commit_sync_config),
        small_to_large: commit_sync_data.repos.get_direction() == CommitSyncDirection::Forward,
        submodule_deps,
        large_repo: large_in_memory_repo,
        strip_commit_extras,
        should_set_committer_info_to_author_info_if_empty,
        add_mapping_to_hg_extra,
    };
    unsafe_sync_commit_in_memory(
        ctx,
        cs,
        in_memory_syncer,
        commit_sync_context,
        expected_version,
    )
    .await?
    .write(ctx, commit_sync_data)
    .await
}

async fn unsafe_sync_commit_pushrebase_impl<'a, R: Repo>(
    ctx: &'a CoreContext,
    source_cs: BonsaiChangeset,
    commit_sync_data: &'a CommitSyncData<R>,
    target_bookmark: Target<BookmarkKey>,
    rewritedates: PushrebaseRewriteDates,
    version: CommitSyncConfigVersion,
    change_mapping_version: Option<CommitSyncConfigVersion>,
    parent_mapping: HashMap<ChangesetId, ChangesetId>,
) -> Result<Option<ChangesetId>, Error> {
    let hash = source_cs.get_changeset_id();
    let (source_repo, target_repo) = commit_sync_data.get_source_target();

    let source_repo_deps = commit_sync_data.get_submodule_deps();

    let parent_selection_hint = CandidateSelectionHint::AncestorOfBookmark(
        target_bookmark.clone(),
        Target(commit_sync_data.get_target_repo().clone()),
    );

    let mut remapped_parents_outcome = vec![];
    for p in source_cs.parents() {
        let maybe_commit_sync_outcome = commit_sync_data
            .get_commit_sync_outcome_with_hint(ctx, Source(p), parent_selection_hint.clone())
            .await?
            .map(|sync_outcome| (sync_outcome, p));
        let commit_sync_outcome = maybe_commit_sync_outcome.ok_or_else(|| {
            format_err!(
                "parent {} has not been remapped yet, therefore can't remap {}",
                p,
                source_cs.get_changeset_id()
            )
        })?;
        remapped_parents_outcome.push(commit_sync_outcome);
    }

    let movers = commit_sync_data.get_movers_by_version(&version).await?;

    let git_submodules_action = get_git_submodule_action_by_version(
        ctx,
        Arc::clone(&commit_sync_data.live_commit_sync_config),
        &version,
        commit_sync_data
            .repos
            .get_source_repo()
            .repo_identity()
            .id(),
        commit_sync_data
            .repos
            .get_target_repo()
            .repo_identity()
            .id(),
    )
    .await?;
    let mut source_cs_mut = source_cs.clone().into_mut();
    if let Some(change_mapping_version) = change_mapping_version {
        set_mapping_change_version(&mut source_cs_mut, change_mapping_version)?;
    }
    let remapped_parents =
        remap_parents(ctx, &source_cs_mut, commit_sync_data, parent_selection_hint).await?;

    let remapped_parents = remapped_parents
        .into_iter()
        .map(|(source_parent, target_parent)| {
            if let Some(new_target) = parent_mapping.get(&target_parent) {
                (source_parent, *new_target)
            } else {
                (source_parent, target_parent)
            }
        })
        .collect();

    let small_repo = commit_sync_data.get_small_repo();
    let (x_repo_submodule_metadata_file_prefix, dangling_submodule_pointers) =
        submodule_metadata_file_prefix_and_dangling_pointers(
            small_repo.repo_identity().id(),
            &version,
            commit_sync_data.live_commit_sync_config.clone(),
        )
        .await?;

    let large_repo = commit_sync_data.get_large_repo();
    let small_repo_id = small_repo.repo_identity().id();
    let fallback_repos = vec![Arc::new(source_repo.clone())]
        .into_iter()
        .chain(source_repo_deps.repos())
        .collect::<Vec<_>>();
    let large_in_memory_repo = InMemoryRepo::from_repo(large_repo, fallback_repos)?;

    let submodule_expansion_data = match &source_repo_deps {
        SubmoduleDeps::ForSync(deps) => Some(SubmoduleExpansionData {
            submodule_deps: deps,
            large_repo: large_in_memory_repo,
            x_repo_submodule_metadata_file_prefix: x_repo_submodule_metadata_file_prefix.as_str(),
            small_repo_id,
            dangling_submodule_pointers,
        }),
        SubmoduleDeps::NotNeeded | SubmoduleDeps::NotAvailable => None,
    };

    let rewrite_res = rewrite_commit(
        ctx,
        source_cs_mut,
        &remapped_parents,
        movers,
        &source_repo,
        Default::default(),
        git_submodules_action,
        submodule_expansion_data,
    )
    .await?;

    match rewrite_res.rewritten {
        None => {
            if remapped_parents_outcome.is_empty() {
                commit_sync_data
                    .set_no_sync_candidate(ctx, hash, version)
                    .await?;
            } else if remapped_parents_outcome.len() == 1 {
                use CommitSyncOutcome::*;
                let (sync_outcome, _) = &remapped_parents_outcome[0];
                match sync_outcome {
                    NotSyncCandidate(version) => {
                        commit_sync_data
                            .set_no_sync_candidate(ctx, hash, version.clone())
                            .await?;
                    }
                    RewrittenAs(cs_id, version) | EquivalentWorkingCopyAncestor(cs_id, version) => {
                        commit_sync_data
                            .update_wc_equivalence_with_version(
                                ctx,
                                hash,
                                Some(*cs_id),
                                version.clone(),
                            )
                            .await?;
                    }
                };
            } else {
                return Err(CrossRepoSyncError::AmbiguousWorkingCopyEquivalent(
                    source_cs.get_changeset_id(),
                )
                .into());
            }

            Ok(None)
        }
        Some(rewritten) => {
            // Sync commit
            let frozen = rewritten.freeze()?;
            let rewritten_list = hashset![frozen];
            let submodule_content_ids = submodule_repos_with_content_ids(
                commit_sync_data.get_submodule_deps(),
                rewrite_res.submodule_expansion_content_ids,
            )?;
            upload_commits(
                ctx,
                rewritten_list.clone().into_iter().collect(),
                &source_repo,
                &target_repo,
                submodule_content_ids,
            )
            .await?;

            let pushrebase_flags = PushrebaseFlags {
                rewritedates: rewritedates == PushrebaseRewriteDates::Yes,
                forbid_p2_root_rebases: false,
                casefolding_check: false,
                recursion_limit: None,
                ..Default::default()
            };
            // We need to run all pushrebase hooks because the're not only validating if the
            // commit should be pushed. Some of them do important housekeeping that we shouldn't
            // pass on.

            let pushrebase_hooks = get_pushrebase_hooks(
                ctx,
                &target_repo,
                &target_bookmark,
                &target_repo.repo_config().pushrebase,
                Some(ForwardSyncedCommitInfo {
                    small_bcs_id: hash,
                    small_repo_id: commit_sync_data
                        .repos
                        .get_source_repo()
                        .repo_identity()
                        .id(),
                    large_repo_id: commit_sync_data
                        .repos
                        .get_target_repo()
                        .repo_identity()
                        .id(),
                    version_name: version.clone(),
                }),
            )
            .await?;

            debug!("Starting pushrebase...");
            let pushrebase_res = do_pushrebase_bonsai(
                ctx,
                &target_repo,
                &pushrebase_flags,
                &target_bookmark,
                &rewritten_list,
                pushrebase_hooks.as_slice(),
                None,
            )
            .await;
            let pushrebase_res = pushrebase_res
                .map_err(|e| Error::from(CrossRepoSyncError::PushrebaseFailure(e)))?;
            debug!(
                "Pushrebase complete: distance: {}, retry_num: {}",
                pushrebase_res.pushrebase_distance.0, pushrebase_res.retry_num.0
            );
            let pushrebased_changeset = pushrebase_res.head;
            Ok(Some(pushrebased_changeset))
        }
    }
}

async fn unsafe_always_rewrite_sync_commit_impl<'a, R: Repo>(
    ctx: &'a CoreContext,
    source_cs_id: ChangesetId,
    commit_sync_data: &'a CommitSyncData<R>,
    maybe_parents: Option<HashMap<ChangesetId, ChangesetId>>,
    sync_config_version: &CommitSyncConfigVersion,
) -> Result<Option<ChangesetId>, Error> {
    let (source_repo, target_repo) = commit_sync_data.get_source_target();

    let submodule_deps = commit_sync_data.get_submodule_deps();
    let movers = commit_sync_data
        .get_movers_by_version(sync_config_version)
        .await?;

    let git_submodules_action = get_git_submodule_action_by_version(
        ctx,
        Arc::clone(&commit_sync_data.live_commit_sync_config),
        sync_config_version,
        commit_sync_data
            .repos
            .get_source_repo()
            .repo_identity()
            .id(),
        commit_sync_data
            .repos
            .get_target_repo()
            .repo_identity()
            .id(),
    )
    .await?;
    let source_cs = source_cs_id.load(ctx, source_repo.repo_blobstore()).await?;

    let source_cs = source_cs.clone().into_mut();
    let remapped_parents = match maybe_parents {
        Some(parents) => parents,
        None => {
            remap_parents(
                ctx,
                &source_cs,
                commit_sync_data,
                CandidateSelectionHint::Only,
            )
            .await?
        } // TODO: check if only is ok
    };

    let small_repo = commit_sync_data.get_small_repo();
    let (x_repo_submodule_metadata_file_prefix, dangling_submodule_pointers) =
        submodule_metadata_file_prefix_and_dangling_pointers(
            small_repo.repo_identity().id(),
            sync_config_version,
            commit_sync_data.live_commit_sync_config.clone(),
        )
        .await?;
    let large_repo = commit_sync_data.get_large_repo();
    let small_repo_id = small_repo.repo_identity().id();
    let fallback_repos = vec![Arc::new(source_repo.clone())]
        .into_iter()
        .chain(submodule_deps.repos())
        .collect::<Vec<_>>();
    let large_in_memory_repo = InMemoryRepo::from_repo(large_repo, fallback_repos)?;
    let submodule_expansion_data = match submodule_deps {
        SubmoduleDeps::ForSync(deps) => Some(SubmoduleExpansionData {
            submodule_deps: deps,
            large_repo: large_in_memory_repo,
            x_repo_submodule_metadata_file_prefix: x_repo_submodule_metadata_file_prefix.as_str(),
            small_repo_id,
            dangling_submodule_pointers,
        }),
        SubmoduleDeps::NotNeeded | SubmoduleDeps::NotAvailable => None,
    };

    let rewrite_res = rewrite_commit(
        ctx,
        source_cs,
        &remapped_parents,
        movers,
        &source_repo,
        Default::default(),
        git_submodules_action,
        submodule_expansion_data,
    )
    .await?;
    match rewrite_res.rewritten {
        None => {
            commit_sync_data
                .set_no_sync_candidate(ctx, source_cs_id, sync_config_version.clone())
                .await?;
            Ok(None)
        }
        Some(rewritten) => {
            // Sync commit
            let frozen = rewritten.freeze()?;
            let frozen_cs_id = frozen.get_changeset_id();
            let submodule_content_ids = submodule_repos_with_content_ids(
                commit_sync_data.get_submodule_deps(),
                rewrite_res.submodule_expansion_content_ids,
            )?;
            upload_commits(
                ctx,
                vec![frozen],
                &source_repo,
                &target_repo,
                submodule_content_ids,
            )
            .await?;

            update_mapping_with_version(
                ctx,
                hashmap! { source_cs_id => frozen_cs_id },
                commit_sync_data,
                sync_config_version,
            )
            .await?;
            Ok(Some(frozen_cs_id))
        }
    }
}
