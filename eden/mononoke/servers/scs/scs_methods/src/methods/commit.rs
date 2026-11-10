/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use borrowed::borrowed;
use bytes::Bytes;
use cloned::cloned;
use context::CoreContext;
use futures::FutureExt;
use futures::pin_mut;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_watchdog::WatchdogExt;
use hooks::HookOutcome;
use hooks::HookResult;
use hooks::LogOnlyRejections;
use itertools::Either;
use mononoke_api::BookmarkKey;
use mononoke_api::CandidateSelectionHintArgs;
use mononoke_api::ChangedPathsRestrictionOptions;
use mononoke_api::ChangesetContext;
use mononoke_api::ChangesetFileOrdering;
use mononoke_api::ChangesetHistoryOptions;
use mononoke_api::ChangesetId;
use mononoke_api::ChangesetLinearHistoryOptions;
use mononoke_api::ChangesetPathContentContext;
use mononoke_api::ChangesetPathDiffContext;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::CopyInfo;
use mononoke_api::DenyingRestriction;
use mononoke_api::FingerprintVersion;
use mononoke_api::MetadataDiff;
use mononoke_api::MononokeError;
use mononoke_api::MononokeRepo;
use mononoke_api::PathVisibility;
use mononoke_api::RateLimitOutcome;
use mononoke_api::Repo;
use mononoke_api::RestrictedPathsPolicy;
use mononoke_api::UnifiedDiff;
use mononoke_api::UnifiedDiffMode;
use mononoke_api::XRepoLookupExactBehaviour;
use mononoke_api::XRepoLookupSyncBehaviour;
use mononoke_api_hg::RepoContextHgExt;
use mononoke_macros::mononoke;
use mononoke_types::path::MPath;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use scs_errors::ServiceErrorResultExt;
use source_control as thrift;

use super::commit_restricted_paths;
use crate::commit_id::map_commit_identities;
use crate::commit_id::map_commit_identity;
use crate::from_request::FromRequest;
use crate::from_request::check_range_and_convert;
use crate::from_request::validate_timestamp;
use crate::history::collect_history;
use crate::into_response::AsyncIntoResponseWith;
use crate::into_response::IntoResponse;
use crate::source_control_impl::SourceControlServiceImpl;

// Magic number used when we want to limit concurrency with buffer_unordered.
const CONCURRENCY_LIMIT: usize = 100;

/// Convert the thrift `run_as` union into a set of Mononoke identities to run
/// hooks as. The `encoded_authenticated_identities` variant carries a
/// Compact-thrift-encoded `AuthenticatedIdentityList` and preserves identity
/// attributes; it is only supported in Meta-internal builds. Rejects a request
/// that resolves to no identities, or a type/data identity with an empty
/// `id_type` or `id_data`, so callers get a clear error instead of a confusing
/// hook result.
fn run_as_identities(
    run_as: thrift::RunAsIdentities,
) -> Result<MononokeIdentitySet, scs_errors::ServiceError> {
    let identities = match run_as {
        thrift::RunAsIdentities::identities(identities) => identities
            .into_iter()
            .map(|id| {
                if id.id_type.is_empty() || id.id_data.is_empty() {
                    return Err(scs_errors::invalid_request(format!(
                        "run_as identity has an empty id_type or id_data: {id:?}"
                    )));
                }
                Ok(MononokeIdentity::from_legacy_type_data(
                    id.id_type, id.id_data,
                ))
            })
            .collect::<Result<MononokeIdentitySet, _>>()?,
        thrift::RunAsIdentities::encoded_authenticated_identities(bytes) => {
            MononokeIdentity::try_from_thrift_compact_bytes(&bytes).map_err(|err| {
                scs_errors::invalid_request(format!(
                    "invalid encoded_authenticated_identities: {err:#}"
                ))
            })?
        }
        thrift::RunAsIdentities::UnknownField(variant) => {
            return Err(
                scs_errors::invalid_request(format!("unknown run_as variant: {variant}")).into(),
            );
        }
    };
    if identities.is_empty() {
        return Err(
            scs_errors::invalid_request("run_as was set but resolved to no identities").into(),
        );
    }
    Ok(identities)
}

/// One requested pair after visibility classification: either a
/// diffable pair or a placeholder for a pair denied on both sides.
/// Placeholders keep response positions aligned with request positions.
enum CommitFileDiffsItem {
    Diff {
        path_diff_context: ChangesetPathDiffContext<Repo>,
        placeholder: bool,
    },
    Omitted,
}

impl CommitFileDiffsItem {
    fn to_stopped_at_pair(&self) -> thrift::CommitFileDiffsStoppedAtPair {
        match self {
            Self::Diff {
                path_diff_context, ..
            } => thrift::CommitFileDiffsStoppedAtPair {
                base_path: path_diff_context
                    .get_new_content()
                    .map(|p| p.path().to_string()),
                other_path: path_diff_context
                    .get_old_content()
                    .map(|p| p.path().to_string()),
                ..Default::default()
            },
            Self::Omitted => thrift::CommitFileDiffsStoppedAtPair::default(),
        }
    }

    async fn total_size(&self) -> Result<u64, scs_errors::ServiceError> {
        let (path_diff_context, placeholder) = match self {
            Self::Diff {
                path_diff_context,
                placeholder,
            } => (path_diff_context, *placeholder),
            Self::Omitted => return Ok(0),
        };
        if placeholder {
            Ok(0)
        } else {
            async fn file_size(
                path: Option<&ChangesetPathContentContext<Repo>>,
            ) -> Result<u64, scs_errors::ServiceError> {
                if let Some(path) = path {
                    if let Some(file) = path.file().await? {
                        return Ok(file.metadata().await?.total_size);
                    }
                }
                Ok(0)
            }
            let (base_size, other_size) = try_join!(
                file_size(path_diff_context.get_new_content()),
                file_size(path_diff_context.get_old_content())
            )?;
            Ok(base_size.saturating_add(other_size))
        }
    }

    async fn response_element(
        &self,
        ctx: &CoreContext,
        format: thrift::DiffFormat,
        context_lines: usize,
        repo_name: &str,
        diff_router: &crate::diff::DiffRouter<'_>,
    ) -> Result<Option<CommitFileDiffsResponseElement>, scs_errors::ServiceError> {
        let (path_diff_context, placeholder) = match self {
            Self::Diff {
                path_diff_context,
                placeholder,
            } => (path_diff_context, *placeholder),
            Self::Omitted => return Ok(None),
        };
        match format {
            thrift::DiffFormat::RAW_DIFF => self
                .raw_diff(
                    ctx,
                    path_diff_context,
                    placeholder,
                    context_lines,
                    repo_name,
                    diff_router,
                )
                .await
                .map(Some),
            thrift::DiffFormat::METADATA_DIFF => self
                .metadata_diff(ctx, path_diff_context, repo_name, diff_router)
                .await
                .map(Some),
            unknown => {
                Err(scs_errors::invalid_request(format!("invalid diff format: {unknown:?}")).into())
            }
        }
    }

    async fn raw_diff(
        &self,
        ctx: &CoreContext,
        path_diff_context: &ChangesetPathDiffContext<Repo>,
        placeholder: bool,
        context_lines: usize,
        repo_name: &str,
        diff_router: &crate::diff::DiffRouter<'_>,
    ) -> Result<CommitFileDiffsResponseElement, scs_errors::ServiceError> {
        let mode = if placeholder {
            UnifiedDiffMode::OmitContent
        } else {
            UnifiedDiffMode::Inline
        };

        let diff = diff_router
            .unified_diff(ctx, repo_name, path_diff_context, mode, context_lines)
            .await?;
        Ok(CommitFileDiffsResponseElement::RawDiff { diff })
    }

    async fn metadata_diff(
        &self,
        ctx: &CoreContext,
        path_diff_context: &ChangesetPathDiffContext<Repo>,
        repo_name: &str,
        diff_router: &crate::diff::DiffRouter<'_>,
    ) -> Result<CommitFileDiffsResponseElement, scs_errors::ServiceError> {
        let metadata_diff = diff_router
            .metadata_diff(ctx, repo_name, path_diff_context)
            .await?;
        Ok(CommitFileDiffsResponseElement::MetadataDiff { metadata_diff })
    }
}

enum CommitFileDiffsResponseElement {
    RawDiff { diff: UnifiedDiff },
    MetadataDiff { metadata_diff: MetadataDiff },
}

impl CommitFileDiffsResponseElement {
    fn size(&self) -> usize {
        match self {
            Self::RawDiff { diff } => diff.raw_diff.len(),
            Self::MetadataDiff { .. } => 1,
        }
    }

    fn into_response_for_item(
        self,
        item: &CommitFileDiffsItem,
    ) -> thrift::CommitFileDiffsResponseElement {
        let (base_path, other_path) = match item {
            CommitFileDiffsItem::Diff {
                path_diff_context, ..
            } => (
                path_diff_context
                    .get_new_content()
                    .map(|p| p.path().to_string()),
                path_diff_context
                    .get_old_content()
                    .map(|p| p.path().to_string()),
            ),
            CommitFileDiffsItem::Omitted => (None, None),
        };
        let diff = match self {
            Self::RawDiff { diff } => Some(diff.into_response()),
            Self::MetadataDiff { metadata_diff } => Some(metadata_diff.into_response()),
        };
        thrift::CommitFileDiffsResponseElement {
            base_path,
            other_path,
            diff,
            ..Default::default()
        }
    }
}

/// Filter enumerated find-files paths by per-path visibility, keeping at
/// most `limit` visible paths and recording an omission for each denied
/// path dropped from the page.
///
/// Recording happens downstream of the concurrency buffer, so buffered
/// lookahead past `limit` is dropped without counting.
fn filter_visible_paths<S, F, Fut>(
    paths: S,
    limit: usize,
    policy: &RestrictedPathsPolicy,
    check: F,
) -> impl Stream<Item = Result<String, MononokeError>> + Send
where
    S: Stream<Item = Result<MPath, MononokeError>> + Send,
    F: FnMut(MPath) -> Fut + Send,
    Fut:
        Future<Output = Result<(Option<String>, Option<DenyingRestriction>), MononokeError>> + Send,
{
    paths
        .map_ok(check)
        .try_buffered(50)
        .try_filter_map(move |(kept, denial)| {
            if let Some(restriction) = denial {
                policy.record_denial(&restriction);
            }
            futures::future::ready(Ok(kept))
        })
        .take(limit)
}

impl SourceControlServiceImpl {
    /// Returns the lowest common ancestor of two commits.
    ///
    /// In case of ambiguity (can happen with multiple merges of the same branches) returns the
    /// common ancestor with lowest id out of those with highest generation number.
    pub(crate) async fn commit_common_base_with(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitCommonBaseWithParams,
    ) -> Result<thrift::CommitLookupResponse, scs_errors::ServiceError> {
        let (changeset, other_changeset) = self
            .repo_changeset_pair(ctx.clone(), &commit, &params.other_commit_id)
            .watched()
            .await?;
        let lca = if params.linear_history_only {
            changeset
                .linear_common_base_with(other_changeset.id())
                .watched()
                .await?
        } else {
            changeset
                .common_base_with(other_changeset.id())
                .watched()
                .await?
        };
        Ok(thrift::CommitLookupResponse {
            exists: lca.is_some(),
            ids: if let Some(lca) = lca {
                Some(
                    map_commit_identity(&lca, &params.identity_schemes)
                        .watched()
                        .await?,
                )
            } else {
                None
            },
            ..Default::default()
        })
    }

    /// Look up commit.
    pub(crate) async fn commit_lookup(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitLookupParams,
    ) -> Result<thrift::CommitLookupResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx, &commit.repo).await?;
        match repo
            .changeset(ChangesetSpecifier::from_request(&commit.id)?)
            .await?
        {
            Some(cs) => {
                let ids = map_commit_identity(&cs, &params.identity_schemes).await?;
                Ok(thrift::CommitLookupResponse {
                    exists: true,
                    ids: Some(ids),
                    ..Default::default()
                })
            }
            None => Ok(thrift::CommitLookupResponse {
                exists: false,
                ids: None,
                ..Default::default()
            }),
        }
    }

    /// Look up several commits. Note that this method doesn't trigger git commit derivation.
    pub(crate) async fn repo_multiple_commit_lookup(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoMultipleCommitLookupParams,
    ) -> Result<thrift::RepoMultipleCommitLookupResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx, &repo).await?;
        let id_and_empty_result_or_ctx = stream::iter(params.commit_ids)
            .map(|commit_id| {
                borrowed!(repo);
                async move {
                    let change_context = repo
                        .changeset(ChangesetSpecifier::from_request(&commit_id)?)
                        .await?;
                    let changeset_ctx_or_error = change_context.map_or_else(
                        || {
                            let response = thrift::CommitLookupResponse {
                                exists: false,
                                ids: None,
                                ..Default::default()
                            };
                            Either::Left(thrift::CommitLookupEntry {
                                commit_id: commit_id.clone(),
                                commit_lookup_response: response,
                                ..Default::default()
                            })
                        },
                        Either::Right,
                    );
                    Ok::<_, scs_errors::ServiceError>((commit_id, changeset_ctx_or_error))
                }
            })
            .buffered(CONCURRENCY_LIMIT)
            .try_collect::<Vec<_>>()
            .await?;

        let mut responses = Vec::new();
        let mut id_changeset_pairs = Vec::new();
        for (id, ctx_or_error) in id_and_empty_result_or_ctx.into_iter() {
            match ctx_or_error {
                Either::Left(lookup_entry) => responses.push(lookup_entry),
                Either::Right(ctx) => id_changeset_pairs.push((id, ctx.id())),
            }
        }

        let id_map = map_commit_identities(
            &repo,
            id_changeset_pairs
                .clone()
                .into_iter()
                .map(|(_, cs)| cs)
                .collect(),
            &params.identity_schemes,
        )
        .await?;

        for (requested_id, cs_id) in id_changeset_pairs {
            let ids: Result<
                BTreeMap<thrift::CommitIdentityScheme, thrift::CommitId>,
                scs_errors::ServiceError,
            > = id_map.get(&cs_id).cloned().ok_or_else(|| {
                scs_errors::internal_error("programming error, commit id not found in id map")
                    .into()
            });
            let response = thrift::CommitLookupResponse {
                exists: true,
                ids: Some(ids?),
                ..Default::default()
            };
            responses.push(thrift::CommitLookupEntry {
                commit_id: requested_id,
                commit_lookup_response: response,
                ..Default::default()
            });
        }

        Ok(thrift::RepoMultipleCommitLookupResponse {
            responses,
            ..Default::default()
        })
    }

    /// Get diff.
    pub(crate) async fn commit_file_diffs(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitFileDiffsParams,
    ) -> Result<thrift::CommitFileDiffsResponse, scs_errors::ServiceError> {
        // Check the path count limit
        if params.paths.len() as i64 > thrift::consts::COMMIT_FILE_DIFFS_PATH_COUNT_LIMIT {
            Err(scs_errors::diff_input_too_many_paths(params.paths.len()))?;
        }

        // Resolve the CommitSpecfier into ChangesetContext
        let (repo, base_commit, other_commit) = match params.other_commit_id {
            Some(other_commit_id) => {
                let (base_commit, other_commit) = self
                    .repo_changeset_pair(ctx.clone(), &commit, &other_commit_id)
                    .await?;
                (
                    base_commit.repo_ctx().clone(),
                    base_commit,
                    Some(other_commit),
                )
            }
            None => {
                let (repo, base_commit) = self.repo_changeset(ctx.clone(), &commit).await?;
                (repo, base_commit, None)
            }
        };
        borrowed!(repo);
        let repo_name = repo.repo().repo_identity.name();

        // Resolve the paths into ChangesetPathContentContext
        // To make it more efficient we do a batch request
        // to resolve all paths into path contexts
        let mut base_commit_paths = Vec::new();
        let mut other_commit_paths = Vec::new();
        let mut subtree_sources = HashMap::new();
        let mut paths = Vec::with_capacity(params.paths.len());
        for path_pair in params.paths {
            let base_path = match path_pair.base_path {
                Some(path) => {
                    let mpath = MPath::try_from(&path)
                        .map_err(|error| MononokeError::InvalidRequest(error.to_string()))
                        .context("invalid base commit path")?;
                    base_commit_paths.push(mpath.clone());
                    Some(mpath)
                }
                None => None,
            };
            let (other_path, source_commit_id, replacement_path) = match (
                &other_commit,
                path_pair.other_path,
                path_pair.subtree_source,
            ) {
                (Some(_other_commit), Some(other_path), Some(subtree_source)) => {
                    let other_mpath = MPath::try_from(&other_path)
                        .map_err(|error| MononokeError::InvalidRequest(error.to_string()))
                        .context("invalid other commit path")?;
                    let source_mpath = MPath::try_from(&subtree_source.path)
                        .map_err(|error| MononokeError::InvalidRequest(error.to_string()))
                        .context("invalid subtree source path")?;
                    subtree_sources
                        .entry(subtree_source.commit_id.clone())
                        .or_insert_with(Vec::new)
                        .push(source_mpath.clone());
                    (
                        Some(source_mpath),
                        Some(subtree_source.commit_id),
                        Some(other_mpath),
                    )
                }
                (Some(_other_commit), Some(other_path), None) => {
                    let mpath = MPath::try_from(&other_path)
                        .map_err(|error| MononokeError::InvalidRequest(error.to_string()))
                        .context("invalid other commit path")?;
                    other_commit_paths.push(mpath.clone());
                    (Some(mpath), None, None)
                }
                _ => (None, None, None),
            };
            paths.push((
                base_path,
                other_path,
                source_commit_id,
                replacement_path,
                CopyInfo::from_request(&path_pair.copy_info)?,
                path_pair.generate_placeholder_diff.unwrap_or(false),
            ));
        }

        // One policy for the whole request: all three resolutions share its
        // omission counter. Under Strict, denials fail the whole request and
        // the assembly below never sees a denied side.
        let policy = RestrictedPathsPolicy::for_scs_request(&ctx, repo_name);
        // The subtree branch moves its policy into a coroutine; the other
        // branches only borrow.
        let subtree_policy = policy.clone();
        let (base_path_contexts, other_path_contexts, subtree_source_path_contexts) = try_join!(
            async {
                let stream = base_commit
                    .paths_with_content(base_commit_paths.into_iter(), &policy)
                    .await?;
                let base_commit_contexts = stream
                    .map_ok(|path_context| (path_context.path().clone(), path_context))
                    .try_collect::<HashMap<_, _>>()
                    .await;
                Ok::<_, scs_errors::ServiceError>(base_commit_contexts?)
            },
            async {
                match &other_commit {
                    None => Ok(HashMap::new()),
                    Some(other_commit) => {
                        let stream = other_commit
                            .paths_with_content(other_commit_paths.into_iter(), &policy)
                            .await?;
                        let other_commit_contexts = stream
                            .map_ok(|path_context| (path_context.path().clone(), path_context))
                            .try_collect::<HashMap<_, _>>()
                            .await;
                        Ok::<_, scs_errors::ServiceError>(other_commit_contexts?)
                    }
                }
            },
            async {
                stream::iter(
                    subtree_sources
                        .into_iter()
                        .map(Ok::<_, scs_errors::ServiceError>),
                )
                .try_filter_map(|(commit_id, paths)| {
                    let policy = subtree_policy.clone();
                    async move {
                        let changeset_specifier = ChangesetSpecifier::from_request(&commit_id)
                            .map_err(|error| MononokeError::InvalidRequest(error.to_string()))
                            .context("invalid target commit id")?;
                        if let Some(changeset) = repo.changeset(changeset_specifier).await? {
                            let stream = changeset
                                .paths_with_content(paths.into_iter(), &policy)
                                .await?;
                            let path_contexts = stream
                                .map_ok(|path_context| (path_context.path().clone(), path_context))
                                .try_collect::<HashMap<_, _>>()
                                .await;
                            Ok(Some((commit_id.clone(), (changeset, path_contexts?))))
                        } else {
                            Ok::<_, scs_errors::ServiceError>(None)
                        }
                    }
                })
                .try_collect::<HashMap<_, _>>()
                .await
            }
        )?;

        // Assemble one diff item per requested pair. A requested path missing
        // from the batch map is either genuinely absent (the "not found"
        // error) or a restricted-path denial (that side becomes None). The
        // singular re-check runs the same visibility check, so the
        // classification is exact; it only runs for missing paths, i.e.
        // rarely. Pairs resolve concurrently with bounded parallelism, in
        // input order.
        let items = stream::iter(paths)
            .map(
                |(
                    base_path,
                    other_path,
                    source_commit_id,
                    replacement_path,
                    copy_info,
                    placeholder,
                )| {
                    let base_path_contexts = &base_path_contexts;
                    let other_path_contexts = &other_path_contexts;
                    let subtree_source_path_contexts = &subtree_source_path_contexts;
                    let base_commit = &base_commit;
                    let other_commit = &other_commit;
                    let commit = &commit;
                    async move {
                        let item = async {
                        let (base_context, base_denied) = match base_path.as_ref() {
                            Some(base_path) => match base_path_contexts.get(base_path) {
                                Some(context) => (Some(context.clone()), false),
                                None => match base_commit
                                    .check_path_visibility(base_path)
                                    .await?
                                {
                                    PathVisibility::Present(_) => {
                                        return Err(scs_errors::invalid_request(format!(
                                            "{base_path} not found in {commit:?}"
                                        ))
                                        .into());
                                    }
                                    PathVisibility::Denied(_) => (None, true),
                                },
                            },
                            None => (None, false),
                        };

                        let (other_context, other_denied) = match (
                            source_commit_id.as_ref(),
                            other_path.as_ref(),
                        ) {
                            (Some(source_commit_id), Some(other_path)) => {
                                match subtree_source_path_contexts.get(source_commit_id) {
                                    Some((source_changeset, contexts)) => {
                                        match contexts.get(other_path) {
                                            Some(context) => (Some(context.clone()), false),
                                            None => match source_changeset
                                                .check_path_visibility(other_path)
                                                .await?
                                            {
                                                PathVisibility::Present(_) => {
                                                    return Err(scs_errors::invalid_request(format!(
                                                        "subtree source path {other_path:?} not found in {source_commit_id:?}"
                                                    ))
                                                    .into());
                                                }
                                                PathVisibility::Denied(_) => (None, true),
                                            },
                                        }
                                    }
                                    None => {
                                        return Err(scs_errors::internal_error(format!(
                                            "subtree source {source_commit_id:?} not found"
                                        ))
                                        .into());
                                    }
                                }
                            }
                            (None, Some(other_path)) => {
                                match other_path_contexts.get(other_path) {
                                    Some(context) => (Some(context.clone()), false),
                                    None => {
                                        let Some(other_commit) = other_commit.as_ref() else {
                                            return Err(scs_errors::internal_error(
                                                "other path requested without other commit",
                                            )
                                            .into());
                                        };
                                        match other_commit
                                            .check_path_visibility(other_path)
                                            .await?
                                        {
                                            PathVisibility::Present(_) => {
                                                return Err(scs_errors::invalid_request(format!(
                                                    "{other_path} not found in {other_commit:?}"
                                                ))
                                                .into());
                                            }
                                            PathVisibility::Denied(_) => (None, true),
                                        }
                                    }
                                }
                            }
                            _ => (None, false),
                        };

                        if base_context.is_none()
                            && other_context.is_none()
                            && (base_denied || other_denied)
                        {
                            // Nothing visible on either side: a placeholder
                            // keeps response positions aligned with request
                            // positions. The omission count above records it.
                            return Ok(CommitFileDiffsItem::Omitted);
                        }

                        // A denied side carries no provenance: drop copy info
                        // rather than leaking the hidden side's path through it.
                        let copy_info =
                            if (base_denied || other_denied) && copy_info != CopyInfo::None {
                                CopyInfo::None
                            } else {
                                copy_info
                            };

                        let path = base_path
                            .or(replacement_path.clone())
                            .or(other_path)
                            .ok_or_else(|| {
                                scs_errors::invalid_request("at least one path must be provided")
                            })?
                            .clone();

                        let path_diff_context = ChangesetPathDiffContext::new_file(
                            base_commit.clone(),
                            path,
                            base_context,
                            other_context,
                            copy_info,
                            replacement_path,
                        )?;
                        Ok::<_, scs_errors::ServiceError>(CommitFileDiffsItem::Diff {
                            path_diff_context,
                            placeholder,
                        })
                        };
                        item.await
                    }
                },
            )
            .buffered(50)
            .try_collect::<Vec<_>>()
            .await?;

        // Check the total file size limit
        let total_input_size = stream::iter(items.iter())
            .map(|item| item.total_size())
            .boxed() // Prevents compiler error
            .buffered(100)
            .try_fold(
                0u64,
                |acc, size| async move { Ok(acc.saturating_add(size)) },
            )
            .await?;

        if total_input_size > thrift::consts::COMMIT_FILE_DIFFS_SIZE_LIMIT as u64 {
            Err(scs_errors::diff_input_too_big(total_input_size))?;
        }

        let context = check_range_and_convert("context", params.context, 0..)?;
        let diff_size_limit: Option<usize> = params
            .diff_size_limit
            .map(|limit| check_range_and_convert("diff_size_limit", limit, 0..))
            .transpose()?;
        let mut size_so_far = 0usize;
        let mut stopped_at_pair = None;

        let remote_diff_config = repo.repo().repo_config.remote_diff_config.clone();
        let path_diffs = stream::iter(items)
            .map(|item| {
                cloned!(ctx, repo_name, remote_diff_config);
                async move {
                    let element = item
                        .response_element(
                            &ctx,
                            params.format,
                            context,
                            repo_name,
                            &self.diff_router(remote_diff_config.as_ref()),
                        )
                        .await?;
                    Ok::<_, scs_errors::ServiceError>((item, element))
                }
            })
            .boxed() // Prevents compiler error
            .buffered(20)
            .try_take_while(|(item, element)| {
                let mut limit_reached = false;
                if let Some(diff_size_limit) = diff_size_limit {
                    size_so_far =
                        size_so_far.saturating_add(element.as_ref().map_or(0, |e| e.size()));
                    if size_so_far > diff_size_limit {
                        limit_reached = true;
                        stopped_at_pair = Some(item.to_stopped_at_pair());
                    }
                }
                async move { Ok(!limit_reached) }
            })
            .map_ok(|(item, element)| match element {
                Some(element) => element.into_response_for_item(&item),
                None => thrift::CommitFileDiffsResponseElement::default(),
            })
            .try_collect()
            .await?;

        policy.set_partial_if_omitted(&ctx);

        Ok(thrift::CommitFileDiffsResponse {
            path_diffs,
            stopped_at_pair,
            partial_info: super::partial_response_info(&policy),
            ..Default::default()
        })
    }

    /// Get commit info.
    pub(crate) async fn commit_info(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitInfoParams,
    ) -> Result<thrift::CommitInfo, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit).await?;
        changeset.into_response_with(&params.identity_schemes).await
    }

    /// Get commit generation.
    pub(crate) async fn commit_generation(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        _params: thrift::CommitGenerationParams,
    ) -> Result<i64, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit).await?;
        Ok(changeset.generation().await?.value() as i64)
    }

    /// Get the content fingerprint of a commit.
    pub(crate) async fn commit_fingerprint(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitFingerprintParams,
    ) -> Result<thrift::CommitFingerprintResponse, scs_errors::ServiceError> {
        let version = match params.version {
            thrift::CommitFingerprintVersion::V1 | thrift::CommitFingerprintVersion::V2 => {
                FingerprintVersion::V2
            }
            other => {
                return Err(scs_errors::ServiceError::from(
                    MononokeError::InvalidRequest(format!(
                        "unsupported fingerprint version: {other:?}"
                    )),
                ));
            }
        };
        let (_repo, changeset) = self.repo_changeset(ctx, &commit).await?;
        let fingerprint = changeset.content_fingerprint(version).await?;
        Ok(thrift::CommitFingerprintResponse {
            fingerprint,
            version: thrift::CommitFingerprintVersion::V2,
            ..Default::default()
        })
    }

    /// Returns `true` if this commit is an ancestor of `other_commit`.
    pub(crate) async fn commit_is_ancestor_of(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitIsAncestorOfParams,
    ) -> Result<bool, scs_errors::ServiceError> {
        let (changeset, other_changeset) = self
            .repo_changeset_pair(ctx, &commit, &params.descendant_commit_id)
            .await?;
        let is_ancestor_of = changeset.is_ancestor_of(other_changeset.id()).await?;
        Ok(is_ancestor_of)
    }

    /// Filter a list of candidate commits to only those that are ancestors
    /// of the target commit. Uses progressive frontier lowering for efficiency.
    pub(crate) async fn commit_filter_ancestors(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitFilterAncestorsParams,
    ) -> Result<thrift::CommitFilterAncestorsResponse, scs_errors::ServiceError> {
        let num_candidates = params.candidate_ancestor_ids.len();
        if num_candidates as i64 > thrift::consts::COMMIT_FILTER_ANCESTORS_MAX_CANDIDATES {
            return Err(scs_errors::invalid_request(format!(
                "too many candidates ({}), limit is {}",
                num_candidates,
                thrift::consts::COMMIT_FILTER_ANCESTORS_MAX_CANDIDATES,
            ))
            .into());
        }

        let (repo, changeset) = self.repo_changeset(ctx, &commit).await?;

        // Resolve all candidate commit IDs to ChangesetIds concurrently
        let candidate_cs_ids: Vec<ChangesetId> = stream::iter(params.candidate_ancestor_ids)
            .map(|candidate_id| {
                borrowed!(repo);
                async move {
                    let cs_ctx = repo
                        .changeset(ChangesetSpecifier::from_request(&candidate_id)?)
                        .await?
                        .ok_or_else(|| scs_errors::commit_not_found(format!("{candidate_id:?}")))?;
                    Ok::<_, scs_errors::ServiceError>(cs_ctx.id())
                }
            })
            .buffered(CONCURRENCY_LIMIT)
            .try_collect()
            .await?;

        // Use the batch filter_ancestors method on the commit graph
        let ancestor_cs_ids = repo
            .commit_graph()
            .filter_ancestors(changeset.ctx(), changeset.id(), candidate_cs_ids)
            .watched()
            .await
            .map_err(MononokeError::from)?;

        // Map to requested identity schemes
        let id_mapping =
            map_commit_identities(&repo, ancestor_cs_ids.clone(), &params.identity_schemes)
                .watched()
                .await?;

        let ancestors = ancestor_cs_ids
            .into_iter()
            .map(|cs_id| id_mapping.get(&cs_id).cloned().unwrap_or_default())
            .collect();

        Ok(thrift::CommitFilterAncestorsResponse {
            ancestors,
            ..Default::default()
        })
    }

    /// Returns `true` if this commit is public
    pub(crate) async fn commit_is_public(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        _params: thrift::CommitIsPublicParams,
    ) -> Result<bool, scs_errors::ServiceError> {
        let (_repo_ctx, changeset) = self.repo_changeset(ctx, &commit).await?;
        Ok(changeset.is_public().await?)
    }

    /// Diff two commits
    pub(crate) async fn commit_compare(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitCompareParams,
    ) -> Result<thrift::CommitCompareResponse, scs_errors::ServiceError> {
        // Check if we should route to the remote diff_service
        let repo_name = &commit.repo.name;
        let remote_diff_config = self
            .configs
            .repo_configs()
            .get(repo_name)
            .and_then(|config| config.remote_diff_config.clone());
        let diff_router = self.diff_router(remote_diff_config.as_ref());
        if diff_router.should_use_remote_commit_compare(repo_name) {
            // Authorize the caller here, as the local route does in `repo_changeset`.
            self.repo(ctx.clone(), &commit.repo).await?;
            return diff_router
                .remote_commit_compare(&ctx, repo_name, commit.id.clone(), params)
                .await;
        }

        let (repo, base_changeset, other_changeset) = match &params.other_commit_id {
            Some(id) => {
                let (base, other) = self.repo_changeset_pair(ctx.clone(), &commit, id).await?;
                let repo = base.repo_ctx().clone();
                (repo, base, Some(other))
            }
            None => {
                let (repo, base) = self.repo_changeset(ctx.clone(), &commit).await?;
                (repo, base, None)
            }
        };

        // Under Strict, denials fail the whole request; under SkipAndCollect,
        // entries denied by restricted-path enforcement are omitted or
        // downgraded, and recorded against their denying restriction.
        let policy = RestrictedPathsPolicy::for_scs_request(&ctx, repo.name());
        let result = commit_compare::operations::commit_compare(
            &ctx,
            &repo,
            base_changeset,
            other_changeset,
            &params,
            &policy,
        )
        .await
        .map_err(|e| match e.downcast::<MononokeError>() {
            Ok(mononoke_err) => scs_errors::ServiceError::from(mononoke_err),
            Err(e) => scs_errors::internal_error(format!("{e:#}")).into(),
        })?;

        policy.set_partial_if_omitted(&ctx);

        // Local path: stamp the verdict from the same count that set the
        // context flag. Remote path returns early above with diff_service's
        // own verdict untouched.
        let mut response = result.response;
        response.partial_info = super::partial_response_info(&policy);
        Ok(response)
    }

    /// Returns files that match the criteria
    pub(crate) async fn commit_find_files(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitFindFilesParams,
    ) -> Result<thrift::CommitFindFilesResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let limit: usize = check_range_and_convert(
            "limit",
            params.limit,
            0..=source_control::COMMIT_FIND_FILES_MAX_LIMIT,
        )?;
        let prefixes: Option<Vec<_>> = match params.prefixes {
            Some(prefixes) => Some(
                prefixes
                    .into_iter()
                    .map(|prefix| {
                        MPath::try_from(&prefix).map_err(|e| {
                            scs_errors::invalid_request(format!("invalid prefix '{prefix}': {e}"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            None => None,
        };
        let ordering = match &params.after {
            Some(after) => {
                let after = Some(MPath::try_from(after).map_err(|e| {
                    scs_errors::invalid_request(format!("invalid continuation path '{after}': {e}"))
                })?);
                ChangesetFileOrdering::Ordered { after }
            }
            None => ChangesetFileOrdering::Unordered,
        };

        let policy = RestrictedPathsPolicy::for_scs_request(&ctx, repo.name());
        let partial = !matches!(policy, RestrictedPathsPolicy::Strict);

        let path_stream = changeset
            .find_files(
                prefixes,
                params.basenames,
                params.basename_suffixes,
                ordering,
            )
            .await?;
        let files: Vec<String> = if partial {
            filter_visible_paths(path_stream, limit, &policy, |path| {
                let changeset = changeset.clone();
                async move {
                    match changeset.check_path_visibility(&path).await {
                        Ok(PathVisibility::Present(_)) => Ok((Some(path.to_string()), None)),
                        Ok(PathVisibility::Denied(restriction)) => Ok((None, Some(restriction))),
                        Err(e) => Err(e),
                    }
                }
            })
            .try_collect()
            .await?
        } else {
            path_stream
                .take(limit)
                .map_ok(|path| path.to_string())
                .try_collect()
                .await?
        };
        policy.set_partial_if_omitted(&ctx);

        Ok(thrift::CommitFindFilesResponse {
            files,
            partial_info: super::partial_response_info(&policy),
            ..Default::default()
        })
    }

    /// Returns files that match the criteria
    pub(crate) async fn commit_find_files_stream(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitFindFilesParams,
    ) -> Result<
        (
            thrift::CommitFindFilesStreamResponse,
            BoxStream<'static, Result<thrift::CommitFindFilesStreamItem, scs_errors::ServiceError>>,
        ),
        scs_errors::ServiceError,
    > {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let limit: usize = check_range_and_convert("limit", params.limit, 0..=i64::MAX)?;
        let prefixes: Option<Vec<_>> = match &params.prefixes {
            Some(prefixes) => Some(
                prefixes
                    .iter()
                    .map(|prefix| {
                        MPath::try_from(prefix).map_err(|e| {
                            scs_errors::invalid_request(format!("invalid prefix '{prefix}': {e}"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            None => None,
        };
        let ordering = match &params.after {
            Some(after) => {
                let after = Some(MPath::try_from(after).map_err(|e| {
                    scs_errors::invalid_request(format!("invalid continuation path '{after}': {e}"))
                })?);
                ChangesetFileOrdering::Ordered { after }
            }
            None => ChangesetFileOrdering::Unordered,
        };

        // When partial responses are not allowed, the first denial fails
        // the whole request. When allowed, denied paths are filtered
        // (counting omissions) and the exact verdict is reported on a
        // terminal item; up front we can only set `nocache`,
        // pessimistically, since items are produced lazily.
        let policy = RestrictedPathsPolicy::for_scs_request(&ctx, repo.name());
        let partial = !matches!(policy, RestrictedPathsPolicy::Strict);
        if partial {
            ctx.set_potential_partial_response();
        }

        let files_stream = (async_stream::stream! {
            // Owned by the generator so the filter closures below can
            // borrow it for the whole block.
            let policy = policy.clone();
            let path_stream = changeset
            .find_files(
                prefixes,
                params.basenames,
                params.basename_suffixes,
                ordering,
            )
            .await?;
            let filtered: BoxStream<'_, Result<String, MononokeError>> = if partial {
                filter_visible_paths(path_stream, limit, &policy, |path| {
                    let changeset = changeset.clone();
                    async move {
                        match changeset.check_path_visibility(&path).await {
                            Ok(PathVisibility::Present(_)) => Ok((Some(path.to_string()), None)),
                            Ok(PathVisibility::Denied(restriction)) => Ok((None, Some(restriction))),
                            Err(e) => Err(e),
                        }
                    }
                })
                .boxed()
            } else {
                path_stream
                    .map_ok(|path| path.to_string())
                    .take(limit)
                    .boxed()
            };
            let s = filtered
            .try_chunks(1000)
            .map_ok(|files| thrift::CommitFindFilesStreamItem {
                files,
                ..Default::default()
            })
            .map_err(|err| scs_errors::ServiceError::from(err.1)).boxed();
            pin_mut!(s);
            while let Some(value) = s.next().await {
                yield value;
            }
            if partial {
                // Terminal verdict item carrying the exact signal.
                // `try_chunks` never yields empty chunks, so an empty file
                // list unambiguously marks this item as terminal; clients
                // that do not read the verdict just see an extra empty item.
                policy.set_partial_if_omitted(&ctx);
                yield Ok(thrift::CommitFindFilesStreamItem {
                    files: vec![],
                    partial_info: super::partial_response_info(&policy),
                    ..Default::default()
                });
            }
        })
        .boxed();

        Ok((
            thrift::CommitFindFilesStreamResponse {
                ..Default::default()
            },
            files_stream,
        ))
    }

    /// Returns the history of a commit
    pub(crate) async fn commit_history(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitHistoryParams,
    ) -> Result<thrift::CommitHistoryResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let (descendants_of, exclude_changeset_and_ancestors) = try_join!(
            async {
                if let Some(descendants_of) = &params.descendants_of {
                    Ok::<_, scs_errors::ServiceError>(Some(
                        self.changeset_id(&repo, descendants_of).await?,
                    ))
                } else {
                    Ok(None)
                }
            },
            async {
                if let Some(exclude_changeset_and_ancestors) =
                    &params.exclude_changeset_and_ancestors
                {
                    Ok::<_, scs_errors::ServiceError>(Some(
                        self.changeset_id(&repo, exclude_changeset_and_ancestors)
                            .await?,
                    ))
                } else {
                    Ok(None)
                }
            }
        )?;

        // Cap `limit` at COMMIT_HISTORY_MAX_LIMIT for most clients, but allow a
        // JustKnob switchval-based allowlist of clients to bypass the cap. Some
        // clients pass i32::MAX as a sentinel for "unlimited"; this gives them
        // a glide path to migrate without breakage. Shared knob across history
        // methods (commit_history, commit_path_history).
        let client_id = ctx.metadata().upstream_client_id();
        let enforce_limit =
            justknobs::eval("scm/mononoke:scs_history_enforce_limit", None, client_id);
        let limit: usize = if enforce_limit {
            check_range_and_convert(
                "limit",
                params.limit,
                0..=source_control::COMMIT_HISTORY_MAX_LIMIT,
            )?
        } else {
            check_range_and_convert("limit", params.limit, 0..)?
        };
        let skip: usize = check_range_and_convert("skip", params.skip, 0..)?;

        // Time filter equal to zero might be mistaken by users for an unset, like None.
        // We will consider negative timestamps as invalid and zeros as unset.
        let after_timestamp = validate_timestamp(params.after_timestamp, "after_timestamp")?;
        let before_timestamp = validate_timestamp(params.before_timestamp, "before_timestamp")?;
        let after_committer_timestamp = validate_timestamp(
            params.after_committer_timestamp,
            "after_committer_timestamp",
        )?;
        let before_committer_timestamp = validate_timestamp(
            params.before_committer_timestamp,
            "before_committer_timestamp",
        )?;

        if let (Some(ats), Some(bts)) = (after_timestamp, before_timestamp) {
            if bts < ats {
                return Err(scs_errors::invalid_request(format!(
                    "after_timestamp ({ats}) cannot be greater than before_timestamp ({bts})",
                ))
                .into());
            }
        }
        if let (Some(ats), Some(bts)) = (after_committer_timestamp, before_committer_timestamp) {
            if bts < ats {
                return Err(scs_errors::invalid_request(format!(
                    "after_committer_timestamp ({ats}) cannot be greater than before_committer_timestamp ({bts})",
                ))
                .into());
            }
        }

        if skip > 0
            && (after_timestamp.is_some()
                || before_timestamp.is_some()
                || after_committer_timestamp.is_some()
                || before_committer_timestamp.is_some())
        {
            return Err(scs_errors::invalid_request(
                "Time filters cannot be applied if skip is not 0".to_string(),
            )
            .into());
        }

        let history_stream = changeset
            .history(ChangesetHistoryOptions {
                until_timestamp: after_timestamp,
                until_committer_timestamp: after_committer_timestamp,
                descendants_of,
                exclude_changeset_and_ancestors,
            })
            .await?;
        let history = collect_history(
            history_stream,
            skip,
            limit,
            before_timestamp,
            after_timestamp,
            before_committer_timestamp,
            after_committer_timestamp,
            params.author,
            params.format,
            &params.identity_schemes,
        )
        .await?;

        Ok(thrift::CommitHistoryResponse {
            history,
            ..Default::default()
        })
    }

    pub async fn commit_linear_history(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitLinearHistoryParams,
    ) -> Result<thrift::CommitLinearHistoryResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let (descendants_of, exclude_changeset_and_ancestors) = try_join!(
            async {
                if let Some(descendants_of) = &params.descendants_of {
                    Ok::<_, scs_errors::ServiceError>(Some(
                        self.changeset_id(&repo, descendants_of).await?,
                    ))
                } else {
                    Ok(None)
                }
            },
            async {
                if let Some(exclude_changeset_and_ancestors) =
                    &params.exclude_changeset_and_ancestors
                {
                    Ok::<_, scs_errors::ServiceError>(Some(
                        self.changeset_id(&repo, exclude_changeset_and_ancestors)
                            .await?,
                    ))
                } else {
                    Ok(None)
                }
            }
        )?;

        let limit: usize = check_range_and_convert(
            "limit",
            params.limit,
            0..=source_control::COMMIT_LINEAR_HISTORY_MAX_LIMIT,
        )?;
        let skip: u64 = check_range_and_convert("skip", params.skip, 0..)?;

        let history_stream = changeset
            .linear_history(ChangesetLinearHistoryOptions {
                descendants_of,
                exclude_changeset_and_ancestors,
                skip,
            })
            .await?;
        let history = collect_history(
            history_stream,
            // We set the skip to 0 as skipping is already done as part of ChangesetContext::linear_history.
            0,
            limit,
            None,
            None,
            None,
            None,
            params.author,
            params.format,
            &params.identity_schemes,
        )
        .await?;

        Ok(thrift::CommitLinearHistoryResponse {
            history,
            ..Default::default()
        })
    }

    pub(crate) async fn commit_list_descendant_bookmarks(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitListDescendantBookmarksParams,
    ) -> Result<thrift::CommitListDescendantBookmarksResponse, scs_errors::ServiceError> {
        let limit = match check_range_and_convert(
            "limit",
            params.limit,
            0..=source_control::COMMIT_LIST_DESCENDANT_BOOKMARKS_MAX_LIMIT,
        )? {
            0 => None,
            limit => Some(limit),
        };
        let prefix = if !params.bookmark_prefix.is_empty() {
            Some(params.bookmark_prefix)
        } else {
            None
        };
        let (repo, changeset) = self.repo_changeset(ctx, &commit).await?;
        let bookmarks = repo
            .list_bookmarks(
                params.include_scratch,
                prefix.as_deref(),
                params.after.as_deref(),
                limit,
            )
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        let continue_after = match limit {
            Some(limit) if bookmarks.len() as u64 >= limit => {
                bookmarks.last().map(|bookmark| bookmark.0.clone())
            }
            _ => None,
        };

        async fn filter_descendant(
            changeset: Arc<ChangesetContext<Repo>>,
            bookmark: (String, ChangesetId),
        ) -> Result<Option<(String, ChangesetId)>, MononokeError> {
            if changeset.is_ancestor_of(bookmark.1).await? {
                Ok(Some(bookmark))
            } else {
                Ok(None)
            }
        }

        let bookmarks = stream::iter(bookmarks)
            .map({
                // Wrap `changeset` in `Arc` so that cloning it to send to
                // the tasks is cheap.
                let changeset = Arc::new(changeset);
                move |bookmark| {
                    let changeset = changeset.clone();
                    async move {
                        mononoke::spawn_task(filter_descendant(changeset, bookmark))
                            .await
                            .map_err(anyhow::Error::from)?
                    }
                }
            })
            .buffered(20)
            .try_fold(Vec::new(), |mut bookmarks, maybe_bookmark| async move {
                if let Some(bookmark) = maybe_bookmark {
                    bookmarks.push(bookmark);
                }
                Ok(bookmarks)
            })
            .await?;

        let ids = bookmarks.iter().map(|(_name, cs_id)| *cs_id).collect();
        let id_mapping = map_commit_identities(&repo, ids, &params.identity_schemes).await?;

        let bookmarks = bookmarks
            .into_iter()
            .map(|(name, cs_id)| (name, id_mapping.get(&cs_id).cloned().unwrap_or_default()))
            .collect();

        Ok(thrift::CommitListDescendantBookmarksResponse {
            bookmarks,
            continue_after,
            ..Default::default()
        })
    }

    pub(crate) async fn commit_run_hooks(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitRunHooksParams,
    ) -> Result<thrift::CommitRunHooksResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let pushvars: Option<HashMap<String, Bytes>> = params
            .pushvars
            .map(|p| p.into_iter().map(|(k, v)| (k, Bytes::from(v))).collect());
        let run_as = params.run_as.map(run_as_identities).transpose()?;
        // Both fields only shape this dry run's reported verdicts; a real
        // push never reads them. Log their use for rollout analysis.
        let log_only_rejections = if params.include_log_only_rejections.unwrap_or(false) {
            LogOnlyRejections::Report
        } else {
            LogOnlyRejections::Suppress
        };
        if params.override_commit_message.is_some()
            || log_only_rejections == LogOnlyRejections::Report
        {
            let mut scuba = ctx.scuba().clone();
            scuba.add(
                "override_commit_message_set",
                params.override_commit_message.is_some(),
            );
            scuba.add(
                "include_log_only_rejections",
                log_only_rejections == LogOnlyRejections::Report,
            );
            scuba.log_with_msg("commit_run_hooks dry-run overrides", None);
        }
        let outcomes = changeset
            .run_hooks(
                params.bookmark,
                pushvars.as_ref(),
                run_as,
                params.override_commit_message,
                log_only_rejections,
            )
            .await?;

        let mut outcomes_map = BTreeMap::new();

        for outcome in outcomes {
            let (name, execution) = match outcome {
                HookOutcome::FileHook(id, exec) => (id.hook_name, exec),
                HookOutcome::BookmarkHook(id, exec) => (id.hook_name, exec),
                HookOutcome::ChangesetHook(id, exec) => (id.hook_name, exec),
            };

            match execution.result {
                HookResult::Accepted => {
                    outcomes_map.entry(name).or_insert_with(|| {
                        thrift::HookOutcome::accepted(thrift::HookOutcomeAccepted {
                            ..Default::default()
                        })
                    });
                }
                HookResult::Rejected(rej) => {
                    let rejection = thrift::HookOutcomeRejected {
                        description: rej.description.to_string(),
                        long_description: rej.long_description,
                        message_name: rej.message.as_ref().map(|message| message.name.clone()),
                        message_params: rej
                            .message
                            .map(|message| message.params.into_iter().collect()),
                        ..Default::default()
                    };

                    match outcomes_map
                        .entry(name)
                        .or_insert_with(|| thrift::HookOutcome::rejections(vec![]))
                    {
                        thrift::HookOutcome::rejections(rejs) => rejs.push(rejection),
                        obj => *obj = thrift::HookOutcome::rejections(vec![rejection]),
                    }
                }
            }
        }

        Ok(thrift::CommitRunHooksResponse {
            outcomes: outcomes_map,
            ..Default::default()
        })
    }

    pub(crate) async fn commit_rate_limit_check(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitRateLimitCheckParams,
    ) -> Result<thrift::CommitRateLimitCheckResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit).await?;
        let bookmark = BookmarkKey::new(&params.bookmark)
            .map_err(|e| scs_errors::invalid_request(format!("Invalid bookmark: {e}")))?;

        let result = changeset.commit_rate_limit_check(&bookmark).await?;

        let rule_results = result
            .rule_results
            .into_iter()
            .map(|r| {
                let outcome = match r.outcome {
                    // `Skipped` (rule not applicable) is reported as `allowed`
                    // on the SCS API.
                    RateLimitOutcome::Allowed | RateLimitOutcome::Skipped => {
                        thrift::CommitRateLimitRuleOutcome::allowed(
                            thrift::CommitRateLimitAllowed {
                                ..Default::default()
                            },
                        )
                    }
                    RateLimitOutcome::Exceeded {
                        total,
                        window_secs,
                        max_commits,
                    } => thrift::CommitRateLimitRuleOutcome::exceeded(
                        thrift::CommitRateLimitExceeded {
                            current_count: total as i64,
                            max_commits: max_commits as i64,
                            window_secs: window_secs as i64,
                            ..Default::default()
                        },
                    ),
                };
                let directories = if r.directories.is_empty() {
                    None
                } else {
                    Some(r.directories)
                };
                thrift::CommitRateLimitRuleResult {
                    rule_name: r.rule_name,
                    outcome,
                    user_filter: r.user_filter,
                    directories,
                    ..Default::default()
                }
            })
            .collect();

        Ok(thrift::CommitRateLimitCheckResponse {
            passed: result.passed,
            rule_results,
            ..Default::default()
        })
    }

    pub(crate) async fn commit_subtree_changes(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitSubtreeChangesParams,
    ) -> Result<thrift::CommitSubtreeChangesResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx, &commit).await?;
        let subtree_changes = changeset.subtree_changes().await?;
        let subtree_changes = subtree_changes
            .into_response_with(&(repo, params.identity_schemes))
            .await?;
        Ok(thrift::CommitSubtreeChangesResponse {
            subtree_changes,
            ..Default::default()
        })
    }

    /// Do a cross-repo lookup to see if a commit exists under a different hash in another repo
    pub(crate) async fn commit_lookup_xrepo(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitLookupXRepoParams,
    ) -> Result<thrift::CommitLookupResponse, scs_errors::ServiceError> {
        let repo = self.repo(ctx.clone(), &commit.repo).await?;
        let other_repo = self.repo(ctx, &params.other_repo).await?;
        let candidate_selection_hint = match params.candidate_selection_hint {
            Some(ref hint) => Some(CandidateSelectionHintArgs::from_request(hint)?),
            None => None,
        };

        let sync_behaviour = if params.no_ondemand_sync {
            XRepoLookupSyncBehaviour::NeverSync
        } else {
            XRepoLookupSyncBehaviour::SyncIfAbsent
        };
        let exact = if params.exact {
            XRepoLookupExactBehaviour::OnlyExactMapping
        } else {
            XRepoLookupExactBehaviour::WorkingCopyEquivalence
        };
        match repo
            .xrepo_commit_lookup(
                &other_repo,
                ChangesetSpecifier::from_request(&commit.id)?,
                candidate_selection_hint,
                sync_behaviour,
                exact,
            )
            .await?
        {
            Some(cs) => {
                let ids = map_commit_identity(&cs, &params.identity_schemes).await?;
                Ok(thrift::CommitLookupResponse {
                    exists: true,
                    ids: Some(ids),
                    ..Default::default()
                })
            }
            None => Ok(thrift::CommitLookupResponse {
                exists: false,
                ids: None,
                ..Default::default()
            }),
        }
    }

    /// Returns the mutation history of a commit
    pub(crate) async fn commit_hg_mutation_history(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitHgMutationHistoryParams,
    ) -> Result<thrift::CommitHgMutationHistoryResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let changeset_id = changeset
            .hg_id()
            .await?
            .ok_or_else(|| scs_errors::invalid_request("commit is not a hg commit".to_string()))?;

        let mutations = repo
            .hg()
            .fetch_mutations(HashSet::from_iter([changeset_id]))
            .await?;

        let hg_mutation_history = match params.format {
            thrift::MutationHistoryFormat::COMMIT_ID => {
                let commit_ids = mutations
                    .into_iter()
                    .flat_map(|mutation| {
                        mutation
                            .predecessors()
                            .map(|pred| thrift::CommitId::hg(pred.as_bytes().to_vec()))
                            .collect::<Vec<_>>()
                    })
                    .collect();
                thrift::HgMutationHistory::commit_ids(commit_ids)
            }
            thrift::MutationHistoryFormat::HG_MUTATION => {
                let hg_mutations = mutations
                    .into_iter()
                    .map(|mutation| thrift::HgMutation {
                        successor: thrift::CommitId::hg(mutation.successor().as_bytes().to_vec()),
                        predecessors: mutation
                            .predecessors()
                            .map(|pred| thrift::CommitId::hg(pred.as_bytes().to_vec()))
                            .collect(),
                        split: mutation
                            .split()
                            .map(|split| thrift::CommitId::hg(split.as_bytes().to_vec()))
                            .collect(),
                        op: mutation.op().to_string(),
                        user: mutation.user().to_string(),
                        date: thrift::DateTime {
                            timestamp: mutation.timestamp(),
                            tz: mutation.timezone(),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                    .collect();
                thrift::HgMutationHistory::hg_mutations(hg_mutations)
            }
            unknown => {
                return Err(scs_errors::invalid_request(format!(
                    "invalid mutation history format: {unknown:?}"
                ))
                .into());
            }
        };

        Ok(thrift::CommitHgMutationHistoryResponse {
            hg_mutation_history,
            ..Default::default()
        })
    }

    /// Returns the git mutation history of a commit, extracted from
    /// predecessor/predecessor-op extra headers on the commit object.
    /// Walks the full predecessor chain by loading each predecessor commit
    /// and reading its headers, similar to how commit_hg_mutation_history
    /// walks the SQL mutation store.
    pub(crate) async fn commit_git_mutation_history(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitGitMutationHistoryParams,
    ) -> Result<thrift::CommitGitMutationHistoryResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;

        // Walk the predecessor chain by following headers hop-by-hop.
        // Depth limit prevents infinite loops from circular references.
        const MAX_CHAIN_DEPTH: usize = 100;
        let mut mutations = Vec::new();
        let mut all_predecessor_ids = Vec::new();
        let mut current_changeset = changeset;

        for _ in 0..MAX_CHAIN_DEPTH {
            let git_extra_headers = current_changeset.git_extra_headers().await?;
            let mutation_entry = git_extra_headers
                .as_ref()
                .and_then(|headers| git_types::mutation::extract_mutation_from_headers(headers));

            let Some(entry) = mutation_entry else {
                break;
            };

            let successor_id = current_changeset
                .git_sha1()
                .await?
                .map(|sha1| thrift::CommitId::git(sha1.as_ref().to_vec()))
                .unwrap_or_else(|| {
                    thrift::CommitId::bonsai(current_changeset.id().as_ref().to_vec())
                });

            let predecessor_ids: Vec<thrift::CommitId> = entry
                .predecessor_bytes()
                .into_iter()
                .map(thrift::CommitId::git)
                .collect();

            all_predecessor_ids.extend(predecessor_ids.clone());

            let next_sha1 = entry.predecessors.first().cloned();

            mutations.push(thrift::GitMutation {
                successor: successor_id,
                predecessors: predecessor_ids,
                op: entry.op,
                ..Default::default()
            });

            // Follow the chain: load the predecessor commit
            match next_sha1 {
                Some(sha1) => {
                    let specifier = mononoke_api::ChangesetSpecifier::GitSha1(sha1);
                    match repo.changeset(specifier).await? {
                        Some(cs) => current_changeset = cs,
                        None => break, // Predecessor not on server
                    }
                }
                None => break,
            }
        }

        let git_mutation_history = match params.format {
            thrift::MutationHistoryFormat::COMMIT_ID => {
                thrift::GitMutationHistory::commit_ids(all_predecessor_ids)
            }
            thrift::MutationHistoryFormat::GIT_MUTATION => {
                thrift::GitMutationHistory::git_mutations(mutations)
            }
            unknown => {
                return Err(scs_errors::invalid_request(format!(
                    "invalid mutation history format: {unknown:?}"
                ))
                .into());
            }
        };

        Ok(thrift::CommitGitMutationHistoryResponse {
            git_mutation_history,
            ..Default::default()
        })
    }

    /// Returns the directory branch clusters for a commit
    pub(crate) async fn commit_directory_branch_clusters(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitDirectoryBranchClustersParams,
    ) -> Result<thrift::CommitDirectoryBranchClustersResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit).await?;

        let limit: usize = check_range_and_convert(
            "limit",
            params.limit,
            1..=source_control::DIRECTORY_BRANCH_CLUSTERS_MAX_LIMIT,
        )?;
        let paths = params
            .paths
            .map(|paths| paths.iter().map(MPath::new).collect::<Result<Vec<_>, _>>())
            .transpose()
            .map_err(|error| MononokeError::InvalidRequest(error.to_string()))?;
        let after = params
            .after_path
            .map(|p| MPath::new(&p))
            .transpose()
            .map_err(|error| MononokeError::InvalidRequest(error.to_string()))?;

        // Take one extra cluster so that we know if the limit has been reached.
        let mut clusters = changeset
            .directory_branch_clusters(paths, after)
            .await?
            .take(limit + 1)
            .collect::<Vec<_>>();

        let mut last_path = None;
        if clusters.len() > limit {
            // We have reached the limit, so remove any additional clusters and
            // set the last path to be returned to the caller.
            clusters.truncate(limit);
            last_path = clusters.last().map(|c| c.cluster_primary.clone());
        }

        Ok(thrift::CommitDirectoryBranchClustersResponse {
            clusters: clusters
                .into_iter()
                .map(|c| thrift::DirectoryBranchCluster {
                    primary_path: c.cluster_primary.to_string(),
                    secondary_paths: c.secondaries.into_iter().map(|p| p.to_string()).collect(),
                    ..Default::default()
                })
                .collect(),
            last_path: last_path.map(|p| p.to_string()),
            ..Default::default()
        })
    }

    /// Returns the raw changed paths recorded in a Bonsai changeset
    pub(crate) async fn commit_changed_paths_approx(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        _params: thrift::CommitChangedPathsApproxParams,
    ) -> Result<thrift::CommitChangedPathsApproxResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit).await?;
        let bonsai = changeset.bonsai_changeset().await?;
        let paths = bonsai
            .file_changes_map()
            .keys()
            .map(|path| path.to_string())
            .collect::<BTreeSet<_>>();

        Ok(thrift::CommitChangedPathsApproxResponse {
            paths,
            ..Default::default()
        })
    }

    /// Query paths for restriction status and access permissions.
    pub(crate) async fn commit_restricted_paths_access(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitRestrictedPathsAccessParams,
    ) -> Result<thrift::CommitRestrictedPathsAccessResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        if commit_restricted_paths::use_mock_api(repo.name())? {
            return Err(scs_errors::not_implemented(
                "commit_restricted_paths_access is not mocked yet".to_string(),
            )
            .into());
        }
        let paths: BTreeSet<String> = params.paths.into_iter().collect();
        commit_restricted_paths::restricted_paths_access_impl(
            &changeset,
            paths,
            params.check_permissions,
        )
        .await
    }

    /// Find all restriction roots under the specified roots.
    pub(crate) async fn commit_find_restricted_paths(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitFindRestrictedPathsParams,
    ) -> Result<
        (
            thrift::CommitFindRestrictedPathsStreamResponse,
            BoxStream<
                'static,
                Result<thrift::CommitFindRestrictedPathsStreamItem, scs_errors::ServiceError>,
            >,
        ),
        scs_errors::ServiceError,
    > {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        if commit_restricted_paths::use_mock_api(repo.name())? {
            return Err(scs_errors::not_implemented(
                "commit_find_restricted_paths is not mocked yet".to_string(),
            )
            .into());
        }

        let filter_roots: BTreeSet<String> = params.roots.into_iter().collect();
        let check_permissions = params.check_permissions.unwrap_or(false);
        let return_only_accessible = params.return_only_accessible.unwrap_or(false);

        let stream = commit_restricted_paths::find_nested_restricted_roots(
            &changeset,
            filter_roots,
            check_permissions,
            return_only_accessible,
        )
        .await?;

        Ok((
            thrift::CommitFindRestrictedPathsStreamResponse::default(),
            stream,
        ))
    }

    /// Query restrictions on every content side of a commit comparison.
    pub(crate) async fn commit_restricted_paths_changes(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitRestrictedPathsChangesParams,
    ) -> Result<thrift::CommitRestrictedPathsChangesResponse, scs_errors::ServiceError> {
        if !commit_restricted_paths::use_new_comparison(&params)? {
            let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
            if commit_restricted_paths::use_mock_api(repo.name())? {
                return Err(scs_errors::not_implemented(
                    "commit_restricted_paths_changes is not mocked yet".to_owned(),
                )
                .into());
            }
            return commit_restricted_paths::bonsai_changes_response(&changeset).await;
        }
        let request = commit_restricted_paths::restriction_comparison_request(params)?;
        let (base_id, page) = match request {
            commit_restricted_paths::RestrictionComparisonRequest::Legacy => (None, None),
            commit_restricted_paths::RestrictionComparisonRequest::Compact { base, page } => {
                (base, Some(page))
            }
        };
        let compact = page.is_some();
        let (changeset, base) = match base_id.as_ref() {
            Some(id) => {
                let (changeset, other) = self.repo_changeset_pair(ctx, &commit, id).await?;
                (changeset, Some(other))
            }
            None => {
                let (_, changeset) = self.repo_changeset(ctx, &commit).await?;
                (changeset, None)
            }
        };
        if commit_restricted_paths::use_mock_api(changeset.repo_ctx().name())? {
            return Err(scs_errors::not_implemented(
                "commit_restricted_paths_changes is not mocked yet".to_string(),
            )
            .into());
        }

        let after_path = page.as_ref().and_then(|page| page.after_path.clone());
        let stream = changeset
            .changed_paths_restriction_info(
                base.as_ref(),
                after_path,
                ChangedPathsRestrictionOptions {
                    check_permissions: true,
                    include_permission_changes: compact,
                    include_subtree_source_changes: compact,
                },
            )
            .boxed()
            .await?;
        match page {
            Some(page) => commit_restricted_paths::compact::from_stream(stream, page.limit).await,
            None => commit_restricted_paths::legacy_comparison_response(stream).await,
        }
    }
}

#[cfg(test)]
mod run_as_tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn typed_variant_maps_to_identities() {
        let run_as = thrift::RunAsIdentities::identities(vec![
            thrift::RunAsIdentity {
                id_type: "USER".to_string(),
                id_data: "alice".to_string(),
                ..Default::default()
            },
            thrift::RunAsIdentity {
                id_type: "SERVICE_IDENTITY".to_string(),
                id_data: "landservice".to_string(),
                ..Default::default()
            },
        ]);
        let identities = run_as_identities(run_as).expect("typed run_as should convert");
        assert_eq!(identities.len(), 2);
        assert!(
            identities
                .iter()
                .any(|id| id.id_type() == "USER" && id.id_data() == "alice")
        );
        assert!(
            identities
                .iter()
                .any(|id| id.id_type() == "SERVICE_IDENTITY" && id.id_data() == "landservice")
        );
    }

    #[mononoke::test]
    fn unknown_variant_is_rejected() {
        assert!(run_as_identities(thrift::RunAsIdentities::UnknownField(7)).is_err());
    }

    #[mononoke::test]
    fn empty_identities_list_is_rejected() {
        assert!(run_as_identities(thrift::RunAsIdentities::identities(vec![])).is_err());
    }

    #[mononoke::test]
    fn empty_id_component_is_rejected() {
        let run_as = thrift::RunAsIdentities::identities(vec![thrift::RunAsIdentity {
            id_type: "USER".to_string(),
            id_data: String::new(),
            ..Default::default()
        }]);
        assert!(run_as_identities(run_as).is_err());
    }
}

#[cfg(test)]
mod find_files_filter_tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use futures::stream;
    use mononoke_api::OmissionCollector;
    use mononoke_macros::mononoke;
    use mononoke_types::NonRootMPath;
    use restricted_paths::DenyingRestriction;

    use super::*;

    fn counting_policy() -> RestrictedPathsPolicy {
        RestrictedPathsPolicy::SkipAndCollect(Arc::new(OmissionCollector::default()))
    }

    fn denying_restriction() -> DenyingRestriction {
        let acl = MononokeIdentity::from_str("REPO_REGION:test_acl").expect("test ACL parses");
        DenyingRestriction {
            restriction_root: Some(NonRootMPath::new("restricted").expect("test root parses")),
            repo_region_acl: acl.clone(),
            permission_request_group: acl,
        }
    }

    fn numbered_paths(n: usize) -> Vec<MPath> {
        (0..n)
            .map(|i| MPath::try_from(format!("file{i:03}").as_str()))
            .collect::<Result<Vec<_>, _>>()
            .expect("test paths are valid")
    }

    #[mononoke::test]
    async fn buffered_lookahead_past_limit_is_not_counted() {
        // The visible head path resolves slowly while 60 denied paths
        // complete immediately behind it: every denial finishes before
        // `take(1)` is satisfied, but none is walked, so none counts.
        // Recording inside the buffered future would count all 60.
        let paths = numbered_paths(61);
        let policy = counting_policy();
        let files: Vec<String> = filter_visible_paths(
            stream::iter(paths.into_iter().map(Ok)),
            1,
            &policy,
            |path| async move {
                if path.to_string() == "file000" {
                    for _ in 0..100 {
                        tokio::task::yield_now().await;
                    }
                    Ok((Some(path.to_string()), None))
                } else {
                    Ok((None, Some(denying_restriction())))
                }
            },
        )
        .try_collect()
        .await
        .expect("filtering succeeds");
        assert_eq!(files, vec!["file000".to_string()]);
        assert_eq!(policy.omitted_count(), Some(0));
        assert_eq!(
            policy.omitted_groups(),
            Some(std::collections::BTreeMap::new()),
        );
    }

    #[mononoke::test]
    async fn walked_denials_are_counted_exactly() {
        // Alternating denied/visible prefix with slow visible items, then
        // denied lookahead: the three walked denials count, the trailing
        // un-yielded completions do not.
        let paths = numbered_paths(66);
        let policy = counting_policy();
        let files: Vec<String> = filter_visible_paths(
            stream::iter(paths.into_iter().map(Ok)),
            3,
            &policy,
            |path| async move {
                let n: usize = path
                    .to_string()
                    .strip_prefix("file")
                    .expect("numbered test path")
                    .parse()
                    .expect("numbered test path");
                if n < 6 && n % 2 == 1 {
                    for _ in 0..100 {
                        tokio::task::yield_now().await;
                    }
                    Ok((Some(path.to_string()), None))
                } else {
                    Ok((None, Some(denying_restriction())))
                }
            },
        )
        .try_collect()
        .await
        .expect("filtering succeeds");
        assert_eq!(files.len(), 3);
        assert_eq!(policy.omitted_count(), Some(3));
        assert_eq!(
            policy.omitted_groups(),
            Some(std::collections::BTreeMap::from([(
                denying_restriction(),
                3
            )])),
        );
    }
}
