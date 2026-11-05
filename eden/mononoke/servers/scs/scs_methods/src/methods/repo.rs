/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::str::FromStr;

use anyhow::anyhow;
use bookmarks::BookmarkKey;
use chrono::DateTime;
use chrono::FixedOffset;
use context::CoreContext;
use derived_data_manager::DerivableType;
use futures::future::try_join_all;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_watchdog::WatchdogExt;
use maplit::btreemap;
use metaconfig_types::CommitIdentityScheme;
use mononoke_api::BookmarkFreshness;
use mononoke_api::ChangesetPrefixSpecifier;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::ChangesetSpecifierPrefixResolution;
use mononoke_api::MononokeError;
use mononoke_api::StoreRequest;
use mononoke_api::SubmoduleExpansionUpdate;
use mononoke_api::SubmoduleExpansionUpdateCommitInfo;
use mononoke_api::repo::Stack;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime as MononokeDateTime;
use mononoke_types::NonRootMPath;
use mononoke_types::hash::GitSha1;
use mononoke_types::hash::Sha1;
use mononoke_types::hash::Sha256;
use repo_authorization::AuthorizationContext;
use repo_identity::RepoIdentityRef;
use scs_errors::ServiceErrorResultExt;
use source_control as thrift;

use crate::commit_id::map_commit_identities;
use crate::commit_id::map_commit_identity;
use crate::from_request::FromRequest;
use crate::from_request::check_range_and_convert;
use crate::from_request::convert_pushvars;
use crate::into_response::AsyncIntoResponseWith;
use crate::source_control_impl::SourceControlServiceImpl;
use crate::specifiers::SpecifierExt;

// Cap concurrent in-flight requests when resolving stacks of thousands of commits.
const CONCURRENCY_LIMIT: usize = 100;

mod create_commit;
mod land_stack;
mod rebase_stack;
mod resolve_p4_file_revision;

impl SourceControlServiceImpl {
    /// Detailed repo info.
    ///
    /// Returns detailed information about a repository.
    pub(crate) async fn repo_info(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        _params: thrift::RepoInfoParams,
    ) -> Result<thrift::RepoInfo, scs_errors::ServiceError> {
        let authz = AuthorizationContext::new_bypass_access_control();
        // Route through `get_or_load_repo_config` so split-loaded repos
        // (only present in the per-tier RepoSpec manifest) resolve correctly.
        let repo_config = self
            .configs
            .get_or_load_repo_config(repo.name.as_str())
            .map_err(|_| scs_errors::repo_not_found(repo.description()))?;
        let repo_name = repo.name.to_string();
        let default_commit_identity_scheme_conf = &repo_config.default_commit_identity_scheme;

        let default_commit_identity_scheme = match default_commit_identity_scheme_conf {
            CommitIdentityScheme::HG => thrift::CommitIdentityScheme::HG,
            CommitIdentityScheme::GIT => thrift::CommitIdentityScheme::GIT,
            CommitIdentityScheme::BONSAI => thrift::CommitIdentityScheme::BONSAI,
            CommitIdentityScheme::P4 => thrift::CommitIdentityScheme::P4_CHANGELIST,
            CommitIdentityScheme::UNKNOWN => thrift::CommitIdentityScheme::UNKNOWN,
        };
        // Repo might not be found due to deep sharding. However all push_redirected repos are shallow sharded, so this should be safe because
        // 1. If the repo is push-redirected, we are forced to have it on every server cause we can't predict where the request might land
        // 2. If the repo is not push-redirected, then the answer here would anyway be none
        let push_redirected_to = match self
            .repo_impl(ctx, &repo, authz, |_| async { Ok(None) })
            .await
        {
            Ok(repo) => match repo
                .push_redirector()
                .await
                .map_err(scs_errors::ServiceError::from)
            {
                Ok(prd) => prd.map(|prd| prd.repo.repo_identity().name().to_string()),
                Err(e) if e.repo_not_found() => None,
                Err(e) => return Err(e),
            },
            Err(e) if e.repo_not_found() => None,
            // However, if there is any other form of error, let's surface that to the user
            Err(e) => return Err(e),
        };

        Ok(thrift::RepoInfo {
            name: repo_name.to_string(),
            default_commit_identity_scheme,
            push_redirected_to,
            acl_name: repo_config.hipster_acl.clone(),
            ..Default::default()
        })
    }

    /// Resolve a bookmark to a changeset.
    ///
    /// Returns whether the bookmark exists, and the IDs of the changeset in
    /// the requested identity schemes.
    pub(crate) async fn repo_resolve_bookmark(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoResolveBookmarkParams,
    ) -> Result<thrift::RepoResolveBookmarkResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx, &repo).await?;

        // If namespaces parameter is provided, check all namespaces
        // and return the first match found (in order of namespaces provided)
        if let Some(namespaces) = &params.namespaces {
            for namespace in namespaces {
                let bookmark_name = if namespace.is_empty() {
                    params.bookmark_name.clone()
                } else {
                    format!("{}{}", namespace, params.bookmark_name)
                };

                if let Some(cs) = repo
                    .resolve_bookmark(
                        &BookmarkKey::new(&bookmark_name).map_err(Into::<MononokeError>::into)?,
                        BookmarkFreshness::MaybeStale,
                    )
                    .await?
                {
                    // Return the first match found
                    let ids = map_commit_identity(&cs, &params.identity_schemes).await?;
                    return Ok(thrift::RepoResolveBookmarkResponse {
                        exists: true,
                        ids: Some(ids),
                        ..Default::default()
                    });
                }
            }

            // No matches found in any namespace
            Ok(thrift::RepoResolveBookmarkResponse {
                exists: false,
                ids: None,
                ..Default::default()
            })
        } else {
            // Original behavior: single bookmark lookup
            match repo
                .resolve_bookmark(
                    &BookmarkKey::new(&params.bookmark_name)
                        .map_err(Into::<MononokeError>::into)?,
                    BookmarkFreshness::MaybeStale,
                )
                .await?
            {
                Some(cs) => {
                    let ids = map_commit_identity(&cs, &params.identity_schemes).await?;
                    Ok(thrift::RepoResolveBookmarkResponse {
                        exists: true,
                        ids: Some(ids),
                        ..Default::default()
                    })
                }
                None => Ok(thrift::RepoResolveBookmarkResponse {
                    exists: false,
                    ids: None,
                    ..Default::default()
                }),
            }
        }
    }

    /// Resolve a prefix and its identity scheme to a changeset.
    ///
    /// Returns the IDs of the changeset in the requested identity schemes.
    /// Suggestions for ambiguous prefixes are not provided for now.
    pub(crate) async fn repo_resolve_commit_prefix(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoResolveCommitPrefixParams,
    ) -> Result<thrift::RepoResolveCommitPrefixResponse, scs_errors::ServiceError> {
        use ChangesetSpecifierPrefixResolution::*;
        type Response = thrift::RepoResolveCommitPrefixResponse;
        type ResponseType = thrift::RepoResolveCommitPrefixResponseType;

        let same_request_response_schemes = params.identity_schemes.len() == 1
            && params.identity_schemes.contains(&params.prefix_scheme);

        let prefix = ChangesetPrefixSpecifier::from_request(&params)?;
        let repo = self.repo(ctx, &repo).await?;

        // If the response requires exactly the same identity scheme as in the request,
        // the general case works but we don't need to pay extra overhead to resolve
        // ChangesetSpecifier to a changeset.

        match repo.resolve_changeset_id_prefix(prefix).await? {
            Single(ChangesetSpecifier::Bonsai(cs_id)) if same_request_response_schemes => {
                Ok(Response {
                    ids: Some(btreemap! {
                        params.prefix_scheme => thrift::CommitId::bonsai(cs_id.as_ref().into())
                    }),
                    resolved_type: ResponseType::RESOLVED,
                    ..Default::default()
                })
            }
            Single(ChangesetSpecifier::Hg(cs_id)) if same_request_response_schemes => {
                Ok(Response {
                    ids: Some(btreemap! {
                        params.prefix_scheme => thrift::CommitId::hg(cs_id.as_ref().into())
                    }),
                    resolved_type: ResponseType::RESOLVED,
                    ..Default::default()
                })
            }
            Single(ChangesetSpecifier::GitSha1(cs_id)) if same_request_response_schemes => {
                Ok(Response {
                    ids: Some(btreemap! {
                        params.prefix_scheme => thrift::CommitId::git(cs_id.as_ref().into())
                    }),
                    resolved_type: ResponseType::RESOLVED,
                    ..Default::default()
                })
            }
            Single(cs_id) => match &repo.changeset(cs_id).await? {
                None => Err(scs_errors::internal_error(
                    "unexpected failure to resolve an existing commit",
                )
                .into()),
                Some(cs) => Ok(Response {
                    ids: Some(map_commit_identity(cs, &params.identity_schemes).await?),
                    resolved_type: ResponseType::RESOLVED,
                    ..Default::default()
                }),
            },
            NoMatch => Ok(Response {
                resolved_type: ResponseType::NOT_FOUND,
                ..Default::default()
            }),
            _ => Ok(Response {
                resolved_type: ResponseType::AMBIGUOUS,
                ..Default::default()
            }),
        }
    }

    /// Comprehensive bookmark info.
    ///
    /// Returns value of the bookmark (both fresh and warm) and the timestamp of
    /// last update.
    pub(crate) async fn repo_bookmark_info(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoBookmarkInfoParams,
    ) -> Result<thrift::RepoBookmarkInfoResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx, &repo).await?;
        let info = repo.bookmark_info(params.bookmark_name).await?;
        Ok(thrift::RepoBookmarkInfoResponse {
            info: match info {
                Some(info) => Some(info.into_response_with(&params.identity_schemes).await?),
                None => None,
            },
            ..Default::default()
        })
    }

    /// List bookmarks.
    pub(crate) async fn repo_list_bookmarks(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoListBookmarksParams,
    ) -> Result<thrift::RepoListBookmarksResponse, scs_errors::ServiceError> {
        let limit = match check_range_and_convert(
            "limit",
            params.limit,
            0..=source_control::REPO_LIST_BOOKMARKS_MAX_LIMIT,
        )? {
            0 => None,
            limit => Some(limit),
        };
        let prefix = if !params.bookmark_prefix.is_empty() {
            Some(params.bookmark_prefix)
        } else {
            None
        };
        let repo = self.repo(ctx.clone(), &repo).watched().await?;
        let bookmarks = repo
            .list_bookmarks(
                params.include_scratch,
                prefix.as_deref(),
                params.after.as_deref(),
                limit,
            )
            .watched()
            .await?
            .try_collect::<Vec<_>>()
            .watched()
            .await?;
        let continue_after = match limit {
            Some(limit) if bookmarks.len() as u64 >= limit => {
                bookmarks.last().map(|bookmark| bookmark.0.clone())
            }
            _ => None,
        };
        let ids = bookmarks.iter().map(|(_name, cs_id)| *cs_id).collect();
        let id_mapping = map_commit_identities(&repo, ids, &params.identity_schemes)
            .watched()
            .await?;
        let bookmarks = bookmarks
            .into_iter()
            .map(|(name, cs_id)| match id_mapping.get(&cs_id) {
                Some(ids) => (name, ids.clone()),
                None => (name, BTreeMap::new()),
            })
            .collect();
        Ok(thrift::RepoListBookmarksResponse {
            bookmarks,
            continue_after,
            ..Default::default()
        })
    }

    /// Build stacks for the given list of heads.
    ///
    /// Returns the IDs of the changeset in the requested identity schemes.
    /// Draft nodes and first public roots.
    /// The changesets are returned in topological order.
    ///
    /// Best effort: missing changesets are skipped,
    ///              building stack up to provided limit.
    pub(crate) async fn repo_stack_info(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoStackInfoParams,
    ) -> Result<thrift::RepoStackInfoResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx, &repo).await?;

        // Check the limit
        let limit = check_range_and_convert(
            "limit",
            params.limit,
            0..=thrift::consts::REPO_STACK_INFO_MAX_LIMIT,
        )?;

        // parse changeset specifiers from params
        let head_specifiers = params
            .heads
            .iter()
            .map(ChangesetSpecifier::from_request)
            .collect::<Result<Vec<_>, _>>()?;

        // convert changeset specifiers to bonsai changeset ids
        // missing changesets are skipped
        #[allow(clippy::filter_map_identity)]
        let heads_ids = try_join_all(
            head_specifiers
                .into_iter()
                .map(|specifier| repo.resolve_specifier(specifier)),
        )
        .await?
        .into_iter()
        .filter_map(std::convert::identity)
        .collect::<Vec<_>>();

        // get stack
        let stack = repo.stack(heads_ids.clone(), limit).await?;

        if stack_limit_too_low(&stack, &heads_ids) {
            Err(scs_errors::limit_too_low(limit))?;
        }

        // The ids in `stack` were just produced by walking the commit graph in
        // `repo.stack`, so they are guaranteed to exist. Skip the redundant
        // existence check that `repo.changeset(Bonsai(_))` would do.
        let draft_commits: Vec<_> = stack
            .draft
            .into_iter()
            .map(|cs_id| repo.changeset_from_existing_id(cs_id))
            .collect();
        let public_parents: Vec<_> = stack
            .public
            .into_iter()
            .map(|cs_id| repo.changeset_from_existing_id(cs_id))
            .collect();
        let leftover_heads: Vec<_> = stack
            .leftover_heads
            .into_iter()
            .map(|cs_id| repo.changeset_from_existing_id(cs_id))
            .collect();

        // generate response
        let schemes = &params.identity_schemes;
        let (mut draft_commits, public_parents, leftover_heads) = try_join!(
            stream::iter(draft_commits)
                .map(|cs| cs.into_response_with(schemes))
                .buffer_unordered(CONCURRENCY_LIMIT)
                .try_collect::<Vec<_>>(),
            stream::iter(public_parents)
                .map(|cs| cs.into_response_with(schemes))
                .buffer_unordered(CONCURRENCY_LIMIT)
                .try_collect::<Vec<_>>(),
            leftover_heads.into_response_with(schemes),
        )?;

        // Need to return the draft commits in topological order to meet the API definition
        // at https://fburl.com/code/a017qoam.
        draft_commits.sort_by_key(|commit| commit.generation);
        draft_commits.reverse();

        Ok(thrift::RepoStackInfoResponse {
            draft_commits,
            public_parents,
            leftover_heads,
            ..Default::default()
        })
    }

    pub(crate) async fn repo_create_bookmark(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoCreateBookmarkParams,
    ) -> Result<thrift::RepoCreateBookmarkResponse, scs_errors::ServiceError> {
        let repo = self
            .repo_for_service(ctx, &repo, params.service_identity)
            .await?;
        let target = &params.target;
        let changeset = repo
            .changeset(ChangesetSpecifier::from_request(target)?)
            .await?
            .ok_or_else(|| scs_errors::commit_not_found(target.to_string()))?;
        let pushvars = convert_pushvars(params.pushvars);

        repo.create_bookmark(
            &BookmarkKey::new(&params.bookmark).map_err(Into::<MononokeError>::into)?,
            changeset.id(),
            pushvars.as_ref(),
            None,
        )
        .await?;
        Ok(thrift::RepoCreateBookmarkResponse {
            ..Default::default()
        })
    }

    pub(crate) async fn repo_move_bookmark(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoMoveBookmarkParams,
    ) -> Result<thrift::RepoMoveBookmarkResponse, scs_errors::ServiceError> {
        let repo = self
            .repo_for_service(ctx, &repo, params.service_identity)
            .await?;
        let target = &params.target;
        let changeset = repo
            .changeset(ChangesetSpecifier::from_request(target)?)
            .await?
            .ok_or_else(|| scs_errors::commit_not_found(target.to_string()))?;
        let old_changeset_id = match &params.old_target {
            Some(old_target) => Some(
                repo.changeset(ChangesetSpecifier::from_request(old_target)?)
                    .await
                    .context("failed to resolve old target")?
                    .ok_or_else(|| scs_errors::commit_not_found(old_target.to_string()))?
                    .id(),
            ),
            None => None,
        };
        let pushvars = convert_pushvars(params.pushvars);

        repo.move_bookmark(
            &BookmarkKey::new(&params.bookmark).map_err(Into::<MononokeError>::into)?,
            changeset.id(),
            old_changeset_id,
            params.allow_non_fast_forward_move,
            pushvars.as_ref(),
            None,
        )
        .await?;
        Ok(thrift::RepoMoveBookmarkResponse {
            ..Default::default()
        })
    }

    pub(crate) async fn repo_delete_bookmark(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoDeleteBookmarkParams,
    ) -> Result<thrift::RepoDeleteBookmarkResponse, scs_errors::ServiceError> {
        let repo = self
            .repo_for_service(ctx, &repo, params.service_identity)
            .await?;
        let old_changeset_id = match &params.old_target {
            Some(old_target) => Some(
                repo.changeset(ChangesetSpecifier::from_request(old_target)?)
                    .await?
                    .ok_or_else(|| scs_errors::commit_not_found(old_target.to_string()))?
                    .id(),
            ),
            None => None,
        };
        let pushvars = convert_pushvars(params.pushvars);

        repo.delete_bookmark(
            &BookmarkKey::new(&params.bookmark).map_err(Into::<MononokeError>::into)?,
            old_changeset_id,
            pushvars.as_ref(),
        )
        .await?;

        Ok(thrift::RepoDeleteBookmarkResponse {
            ..Default::default()
        })
    }

    /// Prepare commits for future operations.
    ///
    /// Perform any necessary pre-processing on the mononoke side to ensure that the commits
    /// are ready to be used later without incurring a performance penalty for repeated
    /// preparation.
    ///
    /// For now, concretely, "preparing" means deriving the provided derived data type for a batch of commits.
    ///
    /// * The dependencies and ancestors of most commits in the batch must have already been derived.
    ///
    /// If these conditions are not met, this endpoint may take an unbounded time to derive all
    /// ancestors and timeout.
    pub(crate) async fn repo_prepare_commits(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoPrepareCommitsParams,
    ) -> Result<thrift::RepoPrepareCommitsResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx.clone(), &repo).await?;
        // Convert thrift commit ids to bonsai changeset ids
        let changesets = try_join_all(
            params
                .commits
                .iter()
                .map(ChangesetSpecifier::from_request)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|specifier| repo.changeset(specifier)),
        )
        .await?;
        let csids = std::iter::zip(params.commits, changesets)
            .map(|(commit, cs)| {
                cs.map(|cs| cs.id())
                    .ok_or_else(|| scs_errors::commit_not_found(commit.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let derived_data_type = DerivableType::from_request(&params.derived_data_type)?;

        const CONCURRENCY: u64 = 1000;
        repo.derive_bulk_locally(&ctx, csids, &[derived_data_type], Some(CONCURRENCY))
            .await?;

        Ok(thrift::RepoPrepareCommitsResponse {
            ..Default::default()
        })
    }

    pub(crate) async fn repo_upload_file_content(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoUploadFileContentParams,
    ) -> Result<thrift::RepoUploadFileContentResponse, scs_errors::ServiceError> {
        let repo = self
            .repo_for_service(ctx, &repo, params.service_identity)
            .await?;
        let mut store_request = StoreRequest::new(
            params
                .data
                .len()
                .try_into()
                .expect("usize should convert to u64"),
        );
        if let Some(expected_content_sha1) = &params.expected_content_sha1 {
            store_request.sha1 = Some(Sha1::from_request(expected_content_sha1)?);
        }
        if let Some(expected_content_sha256) = &params.expected_content_sha256 {
            store_request.sha256 = Some(Sha256::from_request(expected_content_sha256)?);
        }
        if params.expected_content_seeded_blake3.is_some() {
            return Err(scs_errors::invalid_request(
                "Seeded blake3 not yet implemented for file upload",
            )
            .into());
        }

        let id = repo
            .upload_file_content(params.data.into(), &store_request)
            .await?
            .as_ref()
            .to_vec();
        Ok(thrift::RepoUploadFileContentResponse {
            id,
            ..Default::default()
        })
    }

    /// Update a submodule expansion in the large repo, i.e. change the commit
    /// being expanded or delete the expansion entirely.
    pub(crate) async fn repo_update_submodule_expansion(
        &self,
        ctx: CoreContext,
        params: thrift::RepoUpdateSubmoduleExpansionParams,
    ) -> Result<thrift::RepoUpdateSubmoduleExpansionResponse, scs_errors::ServiceError> {
        let large_repo_ctx = self.repo(ctx.clone(), &params.large_repo).await?;

        let base_cs_specifier = ChangesetSpecifier::from_request(&params.base_commit_id)?;
        let base_changeset_id = large_repo_ctx
            .resolve_specifier(base_cs_specifier)
            .await?
            .ok_or_else(|| {
                MononokeError::InvalidRequest(format!(
                    "unknown commit specifier {base_cs_specifier}"
                ))
            })?;
        let submodule_expansion_path =
            NonRootMPath::new(params.submodule_expansion_path.as_bytes())
                .map_err(MononokeError::from)?;

        let commit_info_params = params.commit_info.unwrap_or_default();
        // TODO(T179531912): expose more metadata fields in API
        let author = if commit_info_params.author.is_some() {
            commit_info_params.author
        } else {
            ctx.session().metadata().unix_name().map(String::from)
        };

        let author_date = commit_info_params
            .author_date
            .as_ref()
            .map(<DateTime<FixedOffset>>::from_request)
            .transpose()?
            .map(MononokeDateTime::new);

        let commit_info = SubmoduleExpansionUpdateCommitInfo {
            author,
            message: commit_info_params.message,
            author_date,
        };

        let submodule_expansion_update = match params.new_submodule_commit_or_delete {
            Some(thrift::CommitId::git(commit_hash_data)) => {
                let commit_hash_string = String::from_utf8(commit_hash_data)
                    .map_err(anyhow::Error::from)
                    .map_err(MononokeError::from)
                    .context("Git commit hash encoding")?;
                // TODO(T179531912): support other hashes
                let git_commit_id_bytes = GitSha1::from_str(commit_hash_string.as_str())
                    .map_err(MononokeError::from)
                    .context("GitSha1 creation")?;
                SubmoduleExpansionUpdate::UpdateCommit(git_commit_id_bytes)
            }
            Some(_) => {
                return Err(scs_errors::invalid_request(anyhow!(
                    "New submodule commit is not a valid git commit hash"
                ))
                .into());
            }
            None => SubmoduleExpansionUpdate::Delete,
        };

        let cs_ctx = large_repo_ctx
            .update_submodule_expansion(
                base_changeset_id,
                submodule_expansion_path,
                submodule_expansion_update,
                commit_info,
            )
            .await?;

        let commit_ids = map_commit_identity(&cs_ctx, &params.identity_schemes).await?;

        Ok(thrift::RepoUpdateSubmoduleExpansionResponse {
            ids: commit_ids,
            ..Default::default()
        })
    }
}

fn stack_limit_too_low(stack: &Stack, heads: &[ChangesetId]) -> bool {
    let public: HashSet<&ChangesetId> = stack.public.iter().collect();
    let draft_heads = heads.iter().filter(|cs_id| !public.contains(cs_id)).count();
    stack.draft.len() <= draft_heads && !stack.leftover_heads.is_empty()
}

#[cfg(test)]
mod stack_info_tests {
    use std::sync::Arc;

    use anyhow::Result;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use mononoke_api::Repo;
    use mononoke_api::RepoContext;
    use mononoke_macros::mononoke;
    use phases::PhasesRef;
    use tests_utils::drawdag::extend_from_dag_with_actions;

    use super::*;

    /// `repo_stack_info` must only report `limit_too_low` when the stack made no progress, i.e.
    /// when no draft commit beyond the draft heads it started from fits into the limit. Public
    /// heads (and heads that do not exist, which are skipped before the stack is built) are not
    /// draft heads and must not be counted.
    #[mononoke::fbinit_test]
    async fn test_repo_stack_info_public_head_counts_toward_limit_too_low(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
        let (commits, _) = extend_from_dag_with_actions(
            &ctx,
            &repo,
            r##"
                P-A-B-C
            "##,
        )
        .await?;
        repo.phases()
            .add_reachable_as_public(&ctx, vec![commits["P"]])
            .await?;
        let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;

        // heads = [C (draft), P (public)], limit = 2: C and its parent B are returned and the
        // client can continue from B.
        let heads = vec![commits["C"], commits["P"]];
        let stack = repo_ctx.stack(heads.clone(), 2).await?;
        assert_eq!(stack.draft, vec![commits["C"], commits["B"]]);
        assert_eq!(stack.public, vec![commits["P"]]);
        assert_eq!(stack.leftover_heads, vec![commits["B"]]);
        assert!(!stack_limit_too_low(&stack, &heads));

        // heads = [C, <missing>] resolve to [C] (missing changesets are skipped) before the
        // stack is built and the limit is checked.
        let stack = repo_ctx.stack(vec![commits["C"]], 2).await?;
        assert_eq!(stack.draft, vec![commits["C"], commits["B"]]);
        assert_eq!(stack.leftover_heads, vec![commits["B"]]);
        assert!(!stack_limit_too_low(&stack, &[commits["C"]]));

        // Control: with limit = 1 only the head C itself fits, so the limit really is too low.
        let stack = repo_ctx.stack(heads.clone(), 1).await?;
        assert_eq!(stack.draft, vec![commits["C"]]);
        assert_eq!(stack.leftover_heads, vec![commits["C"]]);
        assert!(stack_limit_too_low(&stack, &heads));
        Ok(())
    }
}
