/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkKey;
use bookmarks_movement::BookmarkKindRestrictions;
pub use bookmarks_movement::PushrebaseOutcome;
use bytes::Bytes;
use cloned::cloned;
use commit_graph::CommitGraphRef;
use cross_repo_sync::CommitSyncOutcome;
use cross_repo_sync::Large;
use cross_repo_sync::Small;
use futures::future;
use futures::future::TryFutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use hook_manager::CrossRepoPushSource;
use hook_manager::PushAuthoredBy;
use hook_manager::manager::HookManagerRef;
use mononoke_types::ChangesetId;
use phases::PhasesRef;
use pushrebase_client::normal_pushrebase;
use repo_blobstore::RepoBlobstoreRef;
use unbundle::PushRedirector;

use crate::MononokeRepo;
use crate::errors::MononokeError;
use crate::repo::BookmarkFreshness;
use crate::repo::RepoContext;

impl<R: MononokeRepo> RepoContext<R> {
    async fn convert_old_bookmark_value(
        &self,
        redirector: &PushRedirector<R>,
        bookmark_value: Large<Option<ChangesetId>>,
    ) -> anyhow::Result<Small<Option<ChangesetId>>> {
        let large_cs_id = match bookmark_value {
            Large(Some(cs_id)) => cs_id,
            Large(None) => return Ok(Small(None)),
        };
        let syncer = &redirector.large_to_small_commit_syncer;
        match syncer
            .get_commit_sync_outcome(self.ctx(), large_cs_id)
            .await?
        {
            None => anyhow::bail!(
                "Bookmark: unexpected absence of CommitSyncOutcome for {large_cs_id} in {syncer:?}"
            ),
            // EquivalentWorkingCopyAncestor is fine because the bookmark commit in the
            // large repo might not have come from the small repo
            Some(CommitSyncOutcome::RewrittenAs(small_cs_id, _))
            | Some(CommitSyncOutcome::EquivalentWorkingCopyAncestor(small_cs_id, _)) => {
                Ok(Small(Some(small_cs_id)))
            }
            Some(outcome) => anyhow::bail!(
                "Bookmark: unexpected CommitSyncOutcome for {large_cs_id} in {syncer:?}: {outcome:?}"
            ),
        }
    }
    async fn convert_outcome(
        &self,
        redirector: &PushRedirector<R>,
        outcome: Large<PushrebaseOutcome>,
        bookmark: BookmarkKey,
    ) -> Result<Small<PushrebaseOutcome>, MononokeError> {
        let ctx = self.ctx();
        let Large(PushrebaseOutcome {
            old_bookmark_value,
            head,
            retry_num,
            rebased_changesets,
            pushrebase_distance,
            log_id,
            merge_resolved_paths,
            merge_summary,
        }) = outcome;
        redirector.ensure_backsynced(ctx, &bookmark, log_id).await?;

        // Convert all fields from large to small repo
        let (Small(old_bookmark_value), head, rebased_changesets) = futures::try_join!(
            self.convert_old_bookmark_value(redirector, Large(old_bookmark_value)),
            redirector.get_large_to_small_commit_equivalent(ctx, head),
            redirector.convert_pushrebased_changesets(ctx, rebased_changesets)
        )?;

        Ok(Small(PushrebaseOutcome {
            old_bookmark_value,
            head,
            retry_num,
            rebased_changesets,
            pushrebase_distance,
            log_id,
            merge_resolved_paths,
            merge_summary,
        }))
    }

    /// Land a stack of commits to a bookmark via pushrebase.
    ///
    /// `base` must be the only merge base of the bookmark and `head`. A
    /// caller whose view of the bookmark is behind passes the same commit.
    /// The commits that descend from `base` and are ancestors of `head` are
    /// rebased onto the bookmark, and must not be public. The other
    /// ancestors of `head` that are not ancestors of the bookmark (the other
    /// side of a merge) land unchanged; those that are not public are
    /// pushed with the rebased commits, so hooks run on them too.
    /// `scm/mononoke:pushrebase_land_stack_validation` can relax or turn off
    /// these checks (see `changesets_to_land`).
    ///
    /// The `MERGE_RESOLUTION_OVERRIDE` pushvar (if present) is parsed
    /// from `pushvars` here and forwarded to `normal_pushrebase`. Absent
    /// → defer to the `pushrebase_enable_merge_resolution` JK; `"true"` /
    /// `"1"` → `ForceOn`; `"false"` / `"0"` → `ForceOff`. Callers don't
    /// need to interpret the pushvar themselves.
    pub async fn land_stack(
        &self,
        bookmark: impl AsRef<str>,
        head: ChangesetId,
        base: ChangesetId,
        pushvars: Option<&HashMap<String, Bytes>>,
        bookmark_restrictions: BookmarkKindRestrictions,
        push_authored_by: PushAuthoredBy,
        force_local_pushrebase: bool,
    ) -> Result<PushrebaseOutcome, MononokeError> {
        self.start_write()?;

        let bookmark = bookmark.as_ref();
        let bookmark = BookmarkKey::new(bookmark)?;

        // Check that base is an ancestor of the head commit, and fail with an
        // appropriate error message if that's not the case.
        if !self
            .repo()
            .commit_graph()
            .is_ancestor(self.ctx(), base, head)
            .await?
        {
            return Err(MononokeError::InvalidRequest(format!(
                "Not a stack: base commit {base} is not an ancestor of head commit {head}",
            )));
        }

        let ctx = self.ctx();
        let blobstore = self.repo().repo_blobstore();
        let changesets: Vec<_> =
            stream::iter(self.changesets_to_land(&bookmark, head, base).await?)
                .map(|cs_id| {
                    cloned!(ctx);
                    async move {
                        cs_id
                            .load(&ctx, blobstore)
                            .map_err(MononokeError::from)
                            .await
                    }
                })
                .buffer_unordered(100)
                .try_collect()
                .await?;

        let push_redirector = self.push_redirector().await?;
        let outcome = if let Some(redirector) = push_redirector.as_ref() {
            // run hooks on small repo
            bookmarks_movement::run_changeset_hooks(
                ctx,
                self.hook_manager().as_ref(),
                &bookmark,
                &changesets,
                pushvars,
                CrossRepoPushSource::NativeToThisRepo,
                push_authored_by,
            )
            .await?;
            // Convert changesets to large repo
            let large_bookmark = redirector.small_to_large_bookmark(&bookmark).await?;
            let small_to_large = redirector
                .sync_uploaded_changesets(ctx, changesets, Some(&large_bookmark))
                .await?;
            // Land the mapped changesets on the large repo
            let outcome = normal_pushrebase(
                self.ctx(),
                &redirector.repo,
                &small_to_large.into_values().collect::<Vec<_>>(),
                &large_bookmark,
                pushvars,
                redirector.repo.hook_manager(),
                CrossRepoPushSource::PushRedirected,
                bookmark_restrictions,
                self.authorization_context(),
                true, // log_new_public_commits_to_scribe
                force_local_pushrebase,
            )
            .await?;
            // Convert response back, finishing the land on the small repo
            self.convert_outcome(redirector, Large(outcome), bookmark)
                .await?
                .0
        } else {
            normal_pushrebase(
                self.ctx(),
                self.repo(),
                &changesets,
                &bookmark,
                pushvars,
                self.hook_manager().as_ref(),
                CrossRepoPushSource::NativeToThisRepo,
                bookmark_restrictions,
                self.authorization_context(),
                true, // log_new_public_commits_to_scribe
                force_local_pushrebase,
            )
            .await?
        };

        Ok(outcome)
    }

    /// The commits that `land_stack` passes to pushrebase, in topological
    /// order: the commits from `base` to `head` and, for a checked stack that
    /// passes, the merged-in commits that are not public.
    ///
    /// `scm/mononoke:pushrebase_land_stack_validation` chooses, per repo, how
    /// the checks apply. At 0, no stack is checked, and only the commits from
    /// `base` to `head` are passed. At 1, every stack is checked; a stack
    /// without merges that fails is logged to Scuba and passed as at 0, and
    /// a stack with merges that fails is rejected. At 2, every stack that
    /// fails is rejected.
    pub(crate) async fn changesets_to_land(
        &self,
        bookmark: &BookmarkKey,
        head: ChangesetId,
        base: ChangesetId,
    ) -> Result<Vec<ChangesetId>, MononokeError> {
        let ctx = self.ctx();
        let commit_graph = self.repo().commit_graph();
        let rebased: Vec<ChangesetId> = commit_graph
            .range_stream(ctx, base, head)
            .await?
            .filter(|cs_id| future::ready(*cs_id != base))
            .collect()
            .await;

        let validation = match justknobs::get_as::<i64>(
            "scm/mononoke:pushrebase_land_stack_validation",
            Some(self.name()),
        ) {
            0 => LandStackValidation::Off,
            1 => LandStackValidation::Shadow,
            _ => LandStackValidation::Enforce,
        };
        if validation == LandStackValidation::Off {
            return Ok(rebased);
        }

        let has_merges = commit_graph
            .many_changeset_parents(ctx, &rebased)
            .await?
            .values()
            .any(|parents| parents.len() > 1);
        let check = self
            .check_land_stack(bookmark, head, base, &rebased)
            .await?;
        let Some(failure) = check.failure else {
            return Ok(check.merged_in.into_iter().chain(rebased).collect());
        };

        let merge_stack = has_merges || check.has_merged_in;
        let mut scuba = ctx.scuba().clone();
        scuba
            .add("land_stack_validation", <&'static str>::from(validation))
            .add("land_stack_validation_check", <&'static str>::from(failure))
            .add("land_stack_merge_stack", merge_stack)
            .add("land_stack_bookmark", bookmark.to_string())
            .add("land_stack_base", base.to_string())
            .add("land_stack_head", head.to_string())
            .add(
                "land_stack_merge_bases",
                check
                    .merge_bases
                    .iter()
                    .map(ChangesetId::to_string)
                    .collect::<Vec<_>>(),
            )
            .add("land_stack_public_commits", check.public.len());
        if validation == LandStackValidation::Shadow && !merge_stack {
            scuba.log_with_msg("land_stack validation would reject", None);
            return Ok(rebased);
        }
        scuba.log_with_msg("land_stack validation rejected", None);
        Err(self
            .land_stack_check_error(bookmark, head, base, &check, failure)
            .await?)
    }

    /// Checks the commits `land_stack` would land: `base` must be the only
    /// merge base of the bookmark and `head`, and `rebased`, the commits from
    /// `base` to `head`, must be non-empty and not public.
    async fn check_land_stack(
        &self,
        bookmark: &BookmarkKey,
        head: ChangesetId,
        base: ChangesetId,
        rebased: &[ChangesetId],
    ) -> Result<LandStackCheck, MononokeError> {
        let ctx = self.ctx();
        let phases = self.repo().phases();
        let public = phases.get_public(ctx, rebased.to_vec(), false).await?;
        let public: Vec<_> = rebased
            .iter()
            .filter(|cs_id| public.contains(cs_id))
            .copied()
            .collect();

        // Without the bookmark there is nothing to merge with; pushrebase
        // decides what happens.
        let bookmark_value = self
            .resolve_bookmark(bookmark, BookmarkFreshness::MostRecent)
            .await?
            .map(|cs| cs.id());
        let (merge_bases, merged_in) = match bookmark_value {
            None => (Vec::new(), Vec::new()),
            Some(bookmark_value) => {
                let not_on_bookmark = self
                    .repo()
                    .commit_graph()
                    .ancestors_difference(ctx, vec![head], vec![bookmark_value])
                    .await?;
                let merge_bases = self
                    .merge_bases(bookmark_value, head, base, &not_on_bookmark)
                    .await?;
                let rebased: HashSet<_> = rebased.iter().copied().collect();
                let merged_in = not_on_bookmark
                    .into_iter()
                    .rev()
                    .filter(|cs_id| !rebased.contains(cs_id))
                    .collect();
                (merge_bases, merged_in)
            }
        };

        let failure = match (bookmark_value, merge_bases.as_slice()) {
            (Some(_), []) => Some(LandStackCheckFailure::NoCommonAncestor),
            (Some(_), [merge_base]) if *merge_base != base => {
                Some(LandStackCheckFailure::MergeBaseMismatch)
            }
            (Some(_), [_, _, ..]) => Some(LandStackCheckFailure::MultipleMergeBases),
            _ if rebased.is_empty() => Some(LandStackCheckFailure::NothingToLand),
            _ if !public.is_empty() => Some(LandStackCheckFailure::PublicCommits),
            _ => None,
        };

        let has_merged_in = !merged_in.is_empty();
        let merged_in = if failure.is_none() {
            let public = phases.get_public(ctx, merged_in.clone(), false).await?;
            merged_in
                .into_iter()
                .filter(|cs_id| !public.contains(cs_id))
                .collect()
        } else {
            Vec::new()
        };

        Ok(LandStackCheck {
            merge_bases,
            public,
            merged_in,
            has_merged_in,
            failure,
        })
    }

    /// The merge bases of the bookmark at `bookmark_value` and `head`, given
    /// `not_on_bookmark`, the ancestors of `head` that are not ancestors of
    /// `bookmark_value`. When `base` is the only one, this checks that
    /// without computing them.
    async fn merge_bases(
        &self,
        bookmark_value: ChangesetId,
        head: ChangesetId,
        base: ChangesetId,
        not_on_bookmark: &[ChangesetId],
    ) -> Result<Vec<ChangesetId>, MononokeError> {
        let ctx = self.ctx();
        let commit_graph = self.repo().commit_graph();

        // Every ancestor of `head` is in `not_on_bookmark` or is an ancestor
        // of a parent of it that is outside it (of `head` itself if it is
        // empty), and those parents are common ancestors of `head` and the
        // bookmark. So `base` is the only merge base if it is a common
        // ancestor and all of those parents are ancestors of `base`.
        let boundary: Vec<ChangesetId> = if not_on_bookmark.is_empty() {
            vec![head]
        } else {
            let excluded: HashSet<_> = not_on_bookmark.iter().copied().collect();
            let mut boundary = HashSet::new();
            for chunk in not_on_bookmark.chunks(1000) {
                for parents in commit_graph
                    .many_changeset_parents(ctx, chunk)
                    .await?
                    .into_values()
                {
                    boundary.extend(parents.into_iter().filter(|p| !excluded.contains(p)));
                }
            }
            boundary.into_iter().collect()
        };
        if commit_graph.is_ancestor(ctx, base, bookmark_value).await?
            && commit_graph
                .filter_ancestors(ctx, base, boundary.clone())
                .await?
                .len()
                == boundary.len()
        {
            return Ok(vec![base]);
        }
        Ok(commit_graph.common_base(ctx, bookmark_value, head).await?)
    }

    /// The error `land_stack` returns for a request that fails its checks.
    async fn land_stack_check_error(
        &self,
        bookmark: &BookmarkKey,
        head: ChangesetId,
        base: ChangesetId,
        check: &LandStackCheck,
        failure: LandStackCheckFailure,
    ) -> Result<MononokeError, MononokeError> {
        let message = match failure {
            LandStackCheckFailure::NoCommonAncestor => {
                let head = self.display_changesets(&[head]).await?.join("");
                format!("cannot land {head} onto '{bookmark}': they have no common ancestor")
            }
            LandStackCheckFailure::MergeBaseMismatch => {
                let ids = self
                    .display_changesets(&[head, check.merge_bases[0], base])
                    .await?;
                format!(
                    "bookmark '{bookmark}' has moved incompatibly with this push: the merge base of '{bookmark}' and {} is {}, but the push is based on {}. Pull and retry",
                    ids[0], ids[1], ids[2],
                )
            }
            LandStackCheckFailure::MultipleMergeBases => {
                let head = self.display_changesets(&[head]).await?.join("");
                format!(
                    "bookmark '{bookmark}' has moved incompatibly with this push: '{bookmark}' and {head} have more than one merge base. Pull and retry"
                )
            }
            LandStackCheckFailure::NothingToLand => {
                let head = self.display_changesets(&[head]).await?.join("");
                if check.merge_bases.is_empty() {
                    format!("nothing to land: {head} is the base of the stack")
                } else {
                    format!("nothing to land: {head} is already an ancestor of '{bookmark}'")
                }
            }
            LandStackCheckFailure::PublicCommits => {
                // As the wire protocol client does, name the first few.
                let mut shown = self
                    .display_changesets(&check.public[..check.public.len().min(3)])
                    .await?;
                if check.public.len() > 3 {
                    shown.push("...".to_string());
                }
                format!("cannot rebase public commits: {}", shown.join(", "))
            }
        };
        Ok(MononokeError::InvalidRequest(message))
    }

    /// `cs_ids` for messages: as Mercurial ids if they all have one, so a
    /// message does not mix the two.
    async fn display_changesets(
        &self,
        cs_ids: &[ChangesetId],
    ) -> Result<Vec<String>, MononokeError> {
        let hg_ids: HashMap<_, _> = self
            .repo()
            .bonsai_hg_mapping()
            .get(self.ctx(), cs_ids.to_vec().into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.hg_cs_id.to_string()))
            .collect();
        let all_hg = cs_ids.iter().all(|cs_id| hg_ids.contains_key(cs_id));
        Ok(cs_ids
            .iter()
            .map(|cs_id| match hg_ids.get(cs_id) {
                Some(hg_id) if all_hg => hg_id.clone(),
                _ => cs_id.to_string(),
            })
            .collect())
    }
}

/// How `land_stack` applies its checks, set by
/// `scm/mononoke:pushrebase_land_stack_validation`. Its `snake_case` name is
/// the value of the `land_stack_validation` Scuba column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum LandStackValidation {
    /// Land the commits from `base` to `head` of every stack, unchecked.
    Off,
    /// Check every stack. Log a failing stack without merges and land it as
    /// `Off` does; reject a failing stack with merges.
    Shadow,
    /// Check every stack, and reject it if it fails.
    Enforce,
}

/// The result of `land_stack`'s checks.
struct LandStackCheck {
    /// The merge bases of the bookmark and `head`. Empty if the bookmark does
    /// not exist.
    merge_bases: Vec<ChangesetId>,
    /// The public commits among those from `base` to `head`.
    public: Vec<ChangesetId>,
    /// The merged-in commits to push with the rebased ones: the ancestors of
    /// `head` that are not ancestors of the bookmark, not between `base` and
    /// `head`, and not public. Empty if a check failed.
    merged_in: Vec<ChangesetId>,
    /// Whether `head` has ancestors outside the bookmark's history that are
    /// not between `base` and `head`, public or not.
    has_merged_in: bool,
    /// The first check that failed.
    failure: Option<LandStackCheckFailure>,
}

/// A check `land_stack` failed. Its `snake_case` name is the value of the
/// `land_stack_validation_check` Scuba column.
#[derive(Clone, Copy, Debug, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum LandStackCheckFailure {
    MergeBaseMismatch,
    MultipleMergeBases,
    NoCommonAncestor,
    NothingToLand,
    PublicCommits,
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn scuba_names() {
        let validations = [
            LandStackValidation::Off,
            LandStackValidation::Shadow,
            LandStackValidation::Enforce,
        ]
        .map(<&'static str>::from);
        assert_eq!(validations, ["off", "shadow", "enforce"]);
        let failures = [
            LandStackCheckFailure::MergeBaseMismatch,
            LandStackCheckFailure::MultipleMergeBases,
            LandStackCheckFailure::NoCommonAncestor,
            LandStackCheckFailure::NothingToLand,
            LandStackCheckFailure::PublicCommits,
        ]
        .map(<&'static str>::from);
        assert_eq!(
            failures,
            [
                "merge_base_mismatch",
                "multiple_merge_bases",
                "no_common_ancestor",
                "nothing_to_land",
                "public_commits",
            ]
        );
    }
}
