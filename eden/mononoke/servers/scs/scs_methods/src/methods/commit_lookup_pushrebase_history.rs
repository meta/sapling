/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cmp::Ordering;
use std::iter::once;

use context::CoreContext;
use metaconfig_types::CommonCommitSyncConfig;
use mononoke_api::Repo;
use mononoke_api::RepoContext;
use mononoke_types::ChangesetId;
use pushrebase_mutation_mapping::PushrebaseMutationMappingRef;
use source_control as thrift;
use synced_commit_mapping::SyncedCommitSourceRepo;

use crate::source_control_impl::SourceControlServiceImpl;

#[derive(Debug, Clone)]
struct RepoChangeset(String, ChangesetId);

/// Pushrebase history is traversed across repo boundaries (xrepo sync), so
/// repos are discovered by name rather than by the caller's specifier.
fn repo_specifier(name: &str) -> thrift::RepoSpecifier {
    thrift::RepoSpecifier {
        name: name.to_string(),
        ..Default::default()
    }
}

impl RepoChangeset {
    fn into_thrift(self) -> thrift::CommitSpecifier {
        let RepoChangeset(name, cs_id) = self;
        thrift::CommitSpecifier {
            repo: repo_specifier(&name),
            id: thrift::CommitId::bonsai(cs_id.as_ref().to_vec()),
            ..Default::default()
        }
    }
}

// Struct that represents current pushrebase history. Provides a simple
// interface which allows extending the history by looking into different
// mappings.
struct RepoChangesetsPushrebaseHistory<'a> {
    ctx: CoreContext,
    scs: &'a SourceControlServiceImpl,
    head: RepoChangeset,
    changesets: Vec<RepoChangeset>,
}

impl<'a> RepoChangesetsPushrebaseHistory<'a> {
    fn new(
        ctx: CoreContext,
        scs: &'a SourceControlServiceImpl,
        repo_name: String,
        changeset: ChangesetId,
    ) -> Self {
        Self {
            ctx,
            scs,
            head: RepoChangeset(repo_name, changeset),
            changesets: vec![],
        }
    }

    fn last(&self) -> RepoChangeset {
        match self.changesets.last() {
            Some(rc) => rc.clone(),
            None => self.head.clone(),
        }
    }

    fn second_last(&self) -> Option<RepoChangeset> {
        match self.changesets.len().cmp(&1) {
            Ordering::Greater => self.changesets.get(self.changesets.len() - 2).cloned(),
            Ordering::Equal => Some(self.head.clone()),
            Ordering::Less => None,
        }
    }

    async fn repo(&self, repo_name: &str) -> Result<RepoContext<Repo>, scs_errors::ServiceError> {
        self.scs
            .repo(self.ctx.clone(), &repo_specifier(repo_name))
            .await
    }

    async fn ensure_head_is_public(&self) -> Result<(), scs_errors::ServiceError> {
        let RepoChangeset(repo_name, bcs_id) = &self.head;
        let repo = self.repo(repo_name).await?;
        let changeset = repo.changeset_from_existing_id(*bcs_id);
        if !changeset.is_public().await? {
            return Err(scs_errors::invalid_request(format!(
                "changeset {bcs_id} is not public, and only public commits could be pushrebased",
            ))
            .into());
        }
        Ok(())
    }

    async fn try_traverse_pushrebase(&mut self) -> Result<bool, scs_errors::ServiceError> {
        let RepoChangeset(repo_name, bcs_id) = self.last();
        let repo = self.repo(&repo_name).await?;
        let bcs_ids = repo
            .repo()
            .pushrebase_mutation_mapping()
            .get_prepushrebase_ids(&self.ctx, bcs_id)
            .await
            .map_err(scs_errors::internal_error)?;
        let mut iter = bcs_ids.iter();
        match (iter.next(), iter.next()) {
            (None, _) => Ok(false),
            (Some(prepushrebase_bcs_id), None) => {
                if *prepushrebase_bcs_id == bcs_id {
                    // When no actual pushrebase is performed, we have the situation where the
                    // prepushrebase commit is the same as the landed commit. We should report that
                    // there was no pushrebase performed to be correct.
                    Ok(false)
                } else {
                    self.changesets
                        .push(RepoChangeset(repo_name.clone(), *prepushrebase_bcs_id));
                    Ok(true)
                }
            }
            (Some(_), Some(_)) => Err(scs_errors::internal_error(format!(
                "pushrebase mapping is ambiguous in repo {repo_name} for {bcs_id}: {bcs_ids:?} (expected only one)",
            ))
            .into()),
        }
    }

    async fn try_traverse_commit_sync(
        &mut self,
        changeset: Option<RepoChangeset>,
    ) -> Result<bool, scs_errors::ServiceError> {
        let RepoChangeset(repo_name, bcs_id) = match changeset {
            Some(changeset) => changeset,
            None => self.last(),
        };

        let repo = self.repo(&repo_name).await?;

        let maybe_common_commit_sync_config = repo
            .live_commit_sync_config()
            .get_common_config_if_exists(repo.repoid())
            .map_err(scs_errors::internal_error)?;

        if let Some(config) = maybe_common_commit_sync_config {
            self.try_traverse_commit_sync_inner(repo, bcs_id, config)
                .await
        } else {
            Ok(false)
        }
    }

    // Helper function that wraps traversing logic from try_traverse_commit_sync.
    // Only needed for better code readability
    async fn try_traverse_commit_sync_inner(
        &mut self,
        repo: RepoContext<Repo>,
        bcs_id: ChangesetId,
        config: CommonCommitSyncConfig,
    ) -> Result<bool, scs_errors::ServiceError> {
        let mut synced_changesets = vec![];
        let (target_repo_ids, expected_sync_origin): (Vec<_>, _) =
            if config.large_repo_id == repo.repoid() {
                (
                    config.small_repos.keys().copied().collect(),
                    SyncedCommitSourceRepo::Small,
                )
            } else {
                (vec![config.large_repo_id], SyncedCommitSourceRepo::Large)
            };

        let mut by_target_repo = repo
            .synced_commit_mapping()
            .get_maybe_stale_many_targets(&self.ctx, repo.repoid(), bcs_id, &target_repo_ids)
            .await
            .map_err(scs_errors::internal_error)?;

        // Driven by target_repo_ids rather than by the returned map, so that the
        // order of synced_changesets - and so of the ambiguity error below - does
        // not vary from request to request.
        for target_repo_id in target_repo_ids {
            let Some(entries) = by_target_repo.remove(&target_repo_id) else {
                continue;
            };
            if let Some(target_repo_name) = self.scs.mononoke.repo_name_from_id(target_repo_id) {
                synced_changesets.extend(entries.into_iter().filter_map(|entry| {
                    let traverse = match entry.maybe_source_repo {
                        // source_repo information can be absent e.g. for old commits but
                        // let's still traverse the mapping because in most cases we will
                        // get the correct result.
                        None => true,
                        Some(source_repo) if source_repo == expected_sync_origin => true,
                        _ => false,
                    };
                    if traverse {
                        Some(RepoChangeset(target_repo_name.clone(), entry.target_bcs_id))
                    } else {
                        None
                    }
                }));
            }
        }

        let mut iter = synced_changesets.iter();

        match (iter.next(), iter.next()) {
            (None, _) => Ok(false),
            (Some(rc), None) => {
                self.changesets.push(rc.clone());
                Ok(true)
            }
            (Some(_), Some(_)) => Err(scs_errors::internal_error(format!(
                "commit sync mapping is ambiguous in repo {} for {}: {:?} (expected only one)",
                repo.name(),
                bcs_id,
                synced_changesets,
            ))
            .into()),
        }
    }

    fn into_thrift(self) -> thrift::CommitLookupPushrebaseHistoryResponse {
        let origin = self.last().into_thrift();
        let history: Vec<_> = once(self.head)
            .chain(self.changesets)
            .map(|rc| rc.into_thrift())
            .collect();
        thrift::CommitLookupPushrebaseHistoryResponse {
            history,
            origin,
            ..Default::default()
        }
    }
}

impl SourceControlServiceImpl {
    // Look up commit history over Pushrebase mutations
    pub(crate) async fn commit_lookup_pushrebase_history(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        _params: thrift::CommitLookupPushrebaseHistoryParams,
    ) -> Result<thrift::CommitLookupPushrebaseHistoryResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let mut history = RepoChangesetsPushrebaseHistory::new(
            ctx,
            self,
            repo.name().to_string(),
            changeset.id(),
        );

        // Ensure that commit is public
        history.ensure_head_is_public().await?;

        let mut pushrebased = false;
        if history.try_traverse_pushrebase().await? {
            pushrebased = true;
        } else if history.try_traverse_commit_sync(None).await? {
            pushrebased = history.try_traverse_pushrebase().await?;
        }

        if pushrebased {
            if !history.try_traverse_commit_sync(None).await? {
                // In some scenarios, like the forward sync, we do not add the mapping for `small
                // repository commit -> prepushrebase commit in large repository`. So, we just use
                // a workaround here which is to look for the `small repository commit -> synced
                // large repository commit` mapping which does always exist.
                history
                    .try_traverse_commit_sync(history.second_last())
                    .await?;
            }
        }

        Ok(history.into_thrift())
    }
}
