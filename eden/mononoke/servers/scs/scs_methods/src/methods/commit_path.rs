/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use borrowed::borrowed;
use bytes::Bytes;
use context::CoreContext;
use dedupmap::DedupMap;
use futures::StreamExt;
use futures::future;
use futures::stream;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_ext::FbStreamExt;
use futures_watchdog::WatchdogExt;
use maplit::btreeset;
use mononoke_api::ChangesetPathHistoryOptions;
use mononoke_api::MononokeError;
use mononoke_api::P4LastChange;
use mononoke_api::PathEntry;
use mononoke_api::RestrictedPathsPolicy;
use mononoke_types::NonRootMPath;
use mononoke_types::P4ChangelistId;
use mononoke_types::blame_v2::BlameParentIndex;
use mononoke_types::path::MPath;
use source_control as thrift;

use crate::commit_id::map_commit_identities;
use crate::commit_id::map_commit_identity;
use crate::from_request::check_range_and_convert;
use crate::from_request::validate_timestamp;
use crate::history::collect_history;
use crate::into_response::IntoResponse;
use crate::source_control_impl::SourceControlServiceImpl;

const BLAME_TITLE_MAX_LENGTH: usize = 128;

/// Fills a last change's Perforce fields. `p4_path` is only set when Perforce's spelling
/// of the path differs from the `requested` one.
fn set_p4_fields(
    last_change: &mut thrift::CommitPathLastChange,
    revision: u32,
    changelist: P4ChangelistId,
    path: &NonRootMPath,
    requested: &str,
) {
    last_change.p4_revision = Some(revision.into());
    last_change.p4_changelist = Some(changelist.id() as i64);
    let path = path.to_string();
    if path != requested {
        last_change.p4_path = Some(path);
    }
}

impl SourceControlServiceImpl {
    /// Determine whether anything exists at this path.
    pub(crate) async fn commit_path_exists(
        &self,
        ctx: CoreContext,
        commit_path: thrift::CommitPathSpecifier,
        _params: thrift::CommitPathExistsParams,
    ) -> Result<thrift::CommitPathExistsResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit_path.commit).await?;
        let path = changeset.path(&commit_path.path).await?;
        Ok(thrift::CommitPathExistsResponse {
            exists: path.exists().await?,
            file_exists: path.is_file().await?,
            tree_exists: path.is_tree().await?,
            ..Default::default()
        })
    }

    /// Returns information about the file or directory at a path in a commit.
    pub(crate) async fn commit_path_info(
        &self,
        ctx: CoreContext,
        commit_path: thrift::CommitPathSpecifier,
        _params: thrift::CommitPathInfoParams,
    ) -> Result<thrift::CommitPathInfoResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit_path.commit).await?;
        let path = changeset.path_with_content(&commit_path.path).await?;
        let response = match path.entry().await? {
            PathEntry::NotPresent => thrift::CommitPathInfoResponse {
                exists: false,
                r#type: None,
                info: None,
                ..Default::default()
            },
            PathEntry::Tree(tree) => {
                let summary = tree.summary().await?;
                let tree_info = (tree.id().clone(), summary).into_response();
                thrift::CommitPathInfoResponse {
                    exists: true,
                    r#type: Some(thrift::EntryType::TREE),
                    info: Some(thrift::EntryInfo::tree(tree_info)),
                    ..Default::default()
                }
            }
            PathEntry::File(file, file_type) => {
                let file_info = file.metadata().await?.into_response();
                thrift::CommitPathInfoResponse {
                    exists: true,
                    r#type: Some(file_type.into_response()),
                    info: Some(thrift::EntryInfo::file(file_info)),
                    ..Default::default()
                }
            }
        };
        Ok(response)
    }

    pub(crate) async fn commit_multiple_path_info(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitMultiplePathInfoParams,
    ) -> Result<thrift::CommitMultiplePathInfoResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let mut paths = vec![];
        for path in params.paths {
            let strpath = path.as_str();
            let mpath = MPath::try_from(strpath)
                .map_err(|error| MononokeError::InvalidRequest(error.to_string()))?;
            paths.push(mpath);
        }

        let policy = RestrictedPathsPolicy::for_scs_request(&ctx, repo.name());
        let stream = changeset
            .paths_with_content(paths.into_iter(), &policy)
            .await?;
        let result = stream
            .map_ok(|context| async move {
                let context_path = context.path().to_string();

                match context.entry().await? {
                    PathEntry::NotPresent => {
                        let not_present_elem = thrift::CommitPathInfoResponse {
                            exists: false,
                            r#type: None,
                            info: None,
                            ..Default::default()
                        };
                        Result::<_, scs_errors::ServiceError>::Ok((context_path, not_present_elem))
                    }
                    PathEntry::Tree(tree) => {
                        let summary = tree.summary().await?;
                        let tree_elem = thrift::CommitPathInfoResponse {
                            exists: true,
                            r#type: Some(thrift::EntryType::TREE),
                            info: Some(thrift::EntryInfo::tree(
                                (tree.id().clone(), summary).into_response(),
                            )),
                            ..Default::default()
                        };
                        Result::<_, scs_errors::ServiceError>::Ok((context_path, tree_elem))
                    }
                    PathEntry::File(file, file_type) => {
                        let metadata = file.metadata().await?;
                        let file_elem = thrift::CommitPathInfoResponse {
                            exists: true,
                            r#type: Some(file_type.into_response()),
                            info: Some(thrift::EntryInfo::file(metadata.into_response())),
                            ..Default::default()
                        };
                        Result::<_, scs_errors::ServiceError>::Ok((context_path, file_elem))
                    }
                }
            })
            .map_err(scs_errors::ServiceError::from)
            .try_buffer_unordered(100)
            .try_collect::<BTreeMap<_, _>>()
            .await?;
        policy.set_partial_if_omitted(&ctx);

        Ok(thrift::CommitMultiplePathInfoResponse {
            path_info: result,
            partial_info: super::partial_response_info(&policy),
            ..Default::default()
        })
    }

    pub(crate) async fn commit_path_blame(
        &self,
        ctx: CoreContext,
        commit_path: thrift::CommitPathSpecifier,
        params: thrift::CommitPathBlameParams,
    ) -> Result<thrift::CommitPathBlameResponse, scs_errors::ServiceError> {
        match params.format {
            thrift::BlameFormat::COMPACT => {
                self.commit_path_blame_compact(ctx.clone(), commit_path, params)
                    .watched()
                    .await
            }
            other_format => Err(scs_errors::invalid_request(format!(
                "unsupported blame format {other_format}"
            ))
            .into()),
        }
    }

    async fn commit_path_blame_compact(
        &self,
        ctx: CoreContext,
        commit_path: thrift::CommitPathSpecifier,
        params: thrift::CommitPathBlameParams,
    ) -> Result<thrift::CommitPathBlameResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self
            .repo_changeset(ctx.clone(), &commit_path.commit)
            .watched()
            .await?;
        borrowed!(repo);
        let path = changeset
            .path_with_history(&commit_path.path)
            .watched()
            .await?;

        let options = params.format_options.unwrap_or_else(|| {
            btreeset! { thrift::BlameFormatOption::INCLUDE_CONTENTS }
        });
        let option_include_contents =
            options.contains(&thrift::BlameFormatOption::INCLUDE_CONTENTS);
        let option_include_title = options.contains(&thrift::BlameFormatOption::INCLUDE_TITLE);
        let option_include_message = options.contains(&thrift::BlameFormatOption::INCLUDE_MESSAGE);
        let option_include_parent = options.contains(&thrift::BlameFormatOption::INCLUDE_PARENT);
        let option_include_commit_numbers =
            options.contains(&thrift::BlameFormatOption::INCLUDE_COMMIT_NUMBERS);

        let follow_mutable_file_history = if justknobs::eval(
            "scm/mononoke:scs_disable_mutable_blame",
            None,
            Some(repo.name()),
        ) {
            false
        } else {
            params.follow_mutable_file_history.unwrap_or(false)
        };

        // Changeset ids in the order they will be returned.
        let mut indexed_csids = Vec::new();

        // Mapped commit ids in that same order.
        let mut commit_ids = Vec::new();

        // The small number suitable for each commit, in that same order.
        let mut commit_numbers = Vec::new();

        // The index into these vectors of each changeset.
        let mut commit_id_indexes = HashMap::new();

        let mut paths = DedupMap::new();
        let mut authors = DedupMap::new();
        let mut dates = DedupMap::new();
        let mut titles = DedupMap::new();
        let mut messages = DedupMap::new();

        // Fetch the blame, and optionally its associated content.
        let (blame, content) = if option_include_contents {
            path.blame_with_content(follow_mutable_file_history)
                .watched()
                .await?
        } else {
            (
                path.blame(follow_mutable_file_history).watched().await?,
                Bytes::new(),
            )
        };

        // Map all the changeset IDs into the requested identity schemes.  Keep a mapping of
        // which bonsai changeset ID corresponds to which mapped commit ID index, so we can look
        // them up later.
        let csids_and_nums = blame
            .changeset_ids()
            .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?
            .collect::<Vec<_>>();
        let csids = csids_and_nums
            .iter()
            .map(|(csid, _)| *csid)
            .collect::<Vec<_>>();
        let mut mapped_commit_ids =
            map_commit_identities(repo, csids.clone(), &params.identity_schemes)
                .watched()
                .await?;
        for (id, num) in csids_and_nums {
            if let Some(mapped_ids) = mapped_commit_ids.remove(&id) {
                let index = commit_ids.len();
                commit_ids.push(mapped_ids);
                commit_numbers.push(num as i32);
                commit_id_indexes.insert(id, index);
                indexed_csids.push(id);
            }
        }

        let replacement_parents = blame
            .replacement_parents()
            .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?
            .collect::<HashMap<_, _>>();
        let replacement_parent_csids = replacement_parents.keys().copied().collect::<Vec<_>>();
        let mut mapped_replacement_parents =
            map_commit_identities(repo, replacement_parent_csids, &params.identity_schemes).await?;
        let replacement_parent_commit_ids = Some(
            replacement_parents
                .iter()
                .flat_map(|(csid, num)| {
                    mapped_replacement_parents
                        .remove(csid)
                        .map(|ids| (*num as i32, ids))
                })
                .collect::<BTreeMap<_, _>>(),
        )
        .filter(|ids| !ids.is_empty());

        // Collect author and date fields from the commit info.
        let info: HashMap<_, _> = future::try_join_all(csids.iter().map(move |csid| async move {
            let changeset = repo.changeset_from_existing_id(*csid);
            let (date, author, message) = try_join!(
                changeset.author_date(),
                changeset.author(),
                changeset.message(),
            )?;
            let title: String = message
                .chars()
                .take(BLAME_TITLE_MAX_LENGTH)
                .take_while(|ch| *ch != '\n')
                .collect();

            Ok::<_, MononokeError>((*csid, (author, date, message, title)))
        }))
        .watched()
        .await?
        .into_iter()
        .collect();

        // Collect parent information for each changeset if requested.
        let parent_commit_ids = if option_include_parent {
            let changeset_parents = repo.many_changeset_parents(csids.clone()).watched().await?;
            let all_parent_csids = changeset_parents
                .values()
                .flatten()
                .collect::<HashSet<_>>()
                .into_iter()
                .copied()
                .collect::<Vec<_>>();
            let parent_commit_ids_map =
                map_commit_identities(repo, all_parent_csids, &params.identity_schemes)
                    .watched()
                    .await?;
            let mut parent_commit_ids = Vec::with_capacity(indexed_csids.len());
            for csid in indexed_csids {
                let parents = changeset_parents.get(&csid).ok_or_else(|| {
                    scs_errors::internal_error(format!("missing parents for {csid}"))
                })?;
                let mut changeset_parent_commit_ids = Vec::with_capacity(parents.len());
                for parent in parents {
                    changeset_parent_commit_ids.push(
                        parent_commit_ids_map
                            .get(parent)
                            .ok_or_else(|| {
                                scs_errors::internal_error(format!(
                                    "missing parent commit ids for {parent}"
                                ))
                            })?
                            .clone(),
                    );
                }
                parent_commit_ids.push(changeset_parent_commit_ids);
            }
            Some(parent_commit_ids)
        } else {
            None
        };

        let mut content_iter = content.as_ref().split(|c| *c == b'\n');

        let lines = blame
            .lines()
            .map_err(|e| MononokeError::InvalidRequest(e.to_string()))?;
        let lines = stream::iter(lines.enumerate())
            .yield_periodically()
            .map(
                |(line, blame_line)| -> Result<_, scs_errors::ServiceError> {
                    let commit_id_index = commit_id_indexes
                        .get(blame_line.changeset_id)
                        .ok_or_else(|| {
                            scs_errors::commit_not_found(format!(
                                "failed to resolve commit: {}",
                                blame_line.changeset_id
                            ))
                        })?;
                    let (author, date, message, title) =
                        info.get(blame_line.changeset_id).ok_or_else(|| {
                            scs_errors::commit_not_found(format!(
                                "failed to resolve commit: {}",
                                blame_line.changeset_id
                            ))
                        })?;
                    let mut thrift_blame_line = thrift::BlameCompactLine {
                        line: (line + 1) as i32,
                        contents: None,
                        commit_id_index: *commit_id_index as i32,
                        path_index: paths.insert(&blame_line.path.to_string()) as i32,
                        author_index: authors.insert(author) as i32,
                        date_index: dates.insert(Cow::Borrowed(date)) as i32,
                        origin_line: (blame_line.origin_offset + 1) as i32,
                        title_index: None,
                        message_index: None,
                        ..Default::default()
                    };
                    if option_include_contents {
                        if let Some(content_line) = content_iter.next() {
                            thrift_blame_line.contents =
                                Some(String::from_utf8_lossy(content_line).into_owned());
                        }
                    }
                    if option_include_title {
                        thrift_blame_line.title_index = Some(titles.insert(title) as i32);
                    }
                    if option_include_message {
                        thrift_blame_line.message_index = Some(messages.insert(message) as i32);
                    }
                    if option_include_parent {
                        if let Some(parent) = &blame_line.parent {
                            match parent
                                .parent
                                .indexed(&replacement_parents)
                                .map_err(scs_errors::internal_error)?
                            {
                                BlameParentIndex::ChangesetParent(index) => {
                                    thrift_blame_line.parent_index = Some(index as i32);
                                }
                                BlameParentIndex::ReplacementParent(index) => {
                                    thrift_blame_line.replacement_parent_index = Some(index as i32);
                                }
                            }
                            thrift_blame_line.parent_start_line = Some((parent.offset + 1) as i32);
                            thrift_blame_line.parent_range_length = Some(parent.length as i32);
                            thrift_blame_line.parent_path_index = parent
                                .renamed_from_path
                                .map(|path| paths.insert(&path.to_string()) as i32);
                        }
                    }
                    Ok(thrift_blame_line)
                },
            )
            .try_collect::<Vec<_>>()
            .await?;

        let paths = paths.into_items();
        let authors = authors.into_items();
        let titles = Some(titles.into_items()).filter(|titles| !titles.is_empty());
        let messages = Some(messages.into_items()).filter(|messages| !messages.is_empty());
        let commit_numbers = option_include_commit_numbers.then_some(commit_numbers);
        let approx_commit_count = option_include_commit_numbers
            .then(|| blame.changeset_count().ok())
            .flatten()
            .map(|c| c as i32);
        let distinct_range_count = Some(
            blame
                .range_count()
                .map_err(|e| MononokeError::InvalidRequest(e.to_string()))? as i32,
        );
        let dates = dates
            .into_items()
            .into_iter()
            .map(|date| thrift::DateTime {
                timestamp: date.timestamp(),
                tz: date.offset().local_minus_utc(),
                ..Default::default()
            })
            .collect();
        let blame = thrift::BlameCompact {
            lines,
            commit_ids,
            paths,
            authors,
            dates,
            titles,
            messages,
            parent_commit_ids,
            commit_numbers,
            approx_commit_count,
            distinct_range_count,
            replacement_parent_commit_ids,
            ..Default::default()
        };

        Ok(thrift::CommitPathBlameResponse {
            blame: thrift::Blame::blame_compact(blame),
            ..Default::default()
        })
    }

    pub(crate) async fn commit_path_history(
        &self,
        ctx: CoreContext,
        commit_path: thrift::CommitPathSpecifier,
        params: thrift::CommitPathHistoryParams,
    ) -> Result<thrift::CommitPathHistoryResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self
            .repo_changeset(ctx.clone(), &commit_path.commit)
            .watched()
            .await?;
        let path = changeset
            .path_with_history(&commit_path.path)
            .watched()
            .await?;
        let (descendants_of, exclude_changeset_and_ancestors) = try_join!(
            async {
                if let Some(descendants_of) = &params.descendants_of {
                    Ok::<_, scs_errors::ServiceError>(Some(
                        self.changeset_id(&repo, descendants_of).watched().await?,
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
                            .watched()
                            .await?,
                    ))
                } else {
                    Ok(None)
                }
            }
        )?;

        // Cap `limit` at COMMIT_PATH_HISTORY_MAX_LIMIT for most clients, but
        // allow a JustKnob switchval-based allowlist of clients to bypass the
        // cap. Some clients pass i32::MAX as a sentinel for "unlimited"; this
        // gives them a glide path to migrate without breakage. Shared knob
        // across history methods (commit_history, commit_path_history).
        let client_id = ctx.metadata().upstream_client_id();
        let enforce_limit =
            justknobs::eval("scm/mononoke:scs_history_enforce_limit", None, client_id);
        let limit: usize = if enforce_limit {
            check_range_and_convert(
                "limit",
                params.limit,
                0..=source_control::COMMIT_PATH_HISTORY_MAX_LIMIT,
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

        let history_stream = path
            .history(ChangesetPathHistoryOptions {
                until_timestamp: after_timestamp.clone(),
                until_committer_timestamp: after_committer_timestamp.clone(),
                descendants_of,
                exclude_changeset_and_ancestors,
                follow_history_across_deletions: params.follow_history_across_deletions,
                follow_mutable_file_history: params.follow_mutable_file_history.unwrap_or(false),
            })
            .watched()
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
        .watched()
        .await?;

        Ok(thrift::CommitPathHistoryResponse {
            history,
            ..Default::default()
        })
    }

    pub(crate) async fn commit_path_last_changed(
        &self,
        ctx: CoreContext,
        commit_path: thrift::CommitPathSpecifier,
        params: thrift::CommitPathLastChangedParams,
    ) -> Result<thrift::CommitPathLastChangedResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit_path.commit).await?;
        let path = changeset.path_with_history(&commit_path.path).await?;
        let (last_changed, exists) = match path.last_modified().await? {
            Some(last_modified) => (last_modified, true),
            None => match path.last_deleted().await? {
                Some(last_deleted) => (last_deleted, false),
                None => {
                    return Ok(thrift::CommitPathLastChangedResponse {
                        last_change: None,
                        ..Default::default()
                    });
                }
            },
        };
        let last_changed_commit =
            map_commit_identity(&last_changed, &params.identity_schemes).await?;
        let mut last_change = thrift::CommitPathLastChange {
            exists,
            last_changed_commit,
            ..Default::default()
        };
        if params.include_p4_revision {
            if let Some(p4) = path.p4_revision(&last_changed, exists).await? {
                set_p4_fields(
                    &mut last_change,
                    p4.revision,
                    p4.changelist,
                    &p4.path,
                    &commit_path.path,
                );
            }
        }
        Ok(thrift::CommitPathLastChangedResponse {
            last_change: Some(last_change),
            ..Default::default()
        })
    }

    pub(crate) async fn commit_path_first_changed(
        &self,
        ctx: CoreContext,
        commit_path: thrift::CommitPathSpecifier,
        params: thrift::CommitPathFirstChangedParams,
    ) -> Result<thrift::CommitPathFirstChangedResponse, scs_errors::ServiceError> {
        let (_repo, changeset) = self.repo_changeset(ctx, &commit_path.commit).await?;
        let path = changeset.path_with_history(&commit_path.path).await?;
        let first_commit = match path
            .first_modified(params.follow_history_across_deletions)
            .await?
        {
            Some(first_modified) => {
                Some(map_commit_identity(&first_modified, &params.identity_schemes).await?)
            }
            None => None,
        };
        Ok(thrift::CommitPathFirstChangedResponse {
            first_commit,
            ..Default::default()
        })
    }

    pub(crate) async fn commit_multiple_path_last_changed(
        &self,
        ctx: CoreContext,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitMultiplePathLastChangedParams,
    ) -> Result<thrift::CommitMultiplePathLastChangedResponse, scs_errors::ServiceError> {
        let (repo, changeset) = self.repo_changeset(ctx.clone(), &commit).await?;
        let mut paths = HashSet::with_capacity(params.paths.len());
        for path in params.paths {
            let strpath = path.as_str();
            let mpath = MPath::try_from(strpath)
                .map_err(|error| MononokeError::InvalidRequest(error.to_string()))?;
            paths.insert(mpath);
        }

        let policy = RestrictedPathsPolicy::for_scs_request(&ctx, repo.name());
        let history_stream = changeset
            .paths_with_history(paths.iter().cloned(), &policy)
            .await?;
        let path_last_modified = history_stream
            .map_ok(|context| async move {
                let context_path = context.path().clone();
                let last_modified = context.last_modified().await?;
                let is_file = context.is_file().await?;
                Ok::<_, scs_errors::ServiceError>((context_path, last_modified, is_file))
            })
            .map_err(scs_errors::ServiceError::from)
            .try_buffer_unordered(100)
            .try_filter_map(|(path, maybe_last_changed, is_file)| async move {
                Ok(maybe_last_changed.map(move |last_changed| (path, (last_changed, is_file))))
            })
            .try_collect::<BTreeMap<_, _>>()
            .await?;

        paths.retain(|path| !path_last_modified.contains_key(path));

        let deleted_stream = changeset.deleted_paths(paths.into_iter(), &policy).await?;
        let path_last_deleted = deleted_stream
            .map_ok(|context| async move {
                let context_path = context.path().clone();
                let last_deleted = context.last_deleted().await?;
                Ok::<_, scs_errors::ServiceError>((context_path, last_deleted))
            })
            .map_err(scs_errors::ServiceError::from)
            .try_buffer_unordered(100)
            .try_filter_map(|(path, maybe_last_changed)| async move {
                Ok(maybe_last_changed.map(move |last_changed| (path, last_changed)))
            })
            .try_collect::<BTreeMap<_, _>>()
            .await?;

        let last_changes = path_last_modified
            .into_iter()
            .map(|(path, (changeset, is_file))| P4LastChange {
                path,
                changeset,
                exists: true,
                is_file,
            })
            .chain(
                path_last_deleted
                    .into_iter()
                    .map(|(path, changeset)| P4LastChange {
                        path,
                        changeset,
                        exists: false,
                        is_file: false,
                    }),
            )
            .collect::<Vec<_>>();

        let changesets = last_changes
            .iter()
            .map(|change| change.changeset.id())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let commit_identities =
            map_commit_identities(&repo, changesets, &params.identity_schemes).await?;

        let mut p4_revisions = if params.include_p4_revision {
            repo.p4_path_revisions(last_changes.clone()).await?
        } else {
            HashMap::new()
        };

        let path_last_change = last_changes
            .into_iter()
            .map(|change| {
                let last_changed_commit = commit_identities
                    .get(&change.changeset.id())
                    .cloned()
                    .unwrap_or_default();
                let mut last_change = thrift::CommitPathLastChange {
                    exists: change.exists,
                    last_changed_commit,
                    ..Default::default()
                };
                let path = change.path.to_string();
                if let Some(p4) = p4_revisions.remove(&change.path) {
                    set_p4_fields(
                        &mut last_change,
                        p4.revision,
                        p4.changelist,
                        &p4.path,
                        &path,
                    );
                }
                (path, last_change)
            })
            .collect();

        policy.set_partial_if_omitted(&ctx);

        Ok(thrift::CommitMultiplePathLastChangedResponse {
            path_last_change,
            partial_info: super::partial_response_info(&policy),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn test_set_p4_fields_names_the_path_only_when_perforce_spells_it_differently() {
        let path = NonRootMPath::new("Dir/File.txt").expect("valid path");
        let mut last_change = thrift::CommitPathLastChange::default();
        set_p4_fields(
            &mut last_change,
            7,
            P4ChangelistId::new(41002),
            &path,
            "Dir/File.txt",
        );
        assert_eq!(last_change.p4_revision, Some(7));
        assert_eq!(last_change.p4_changelist, Some(41002));
        assert_eq!(last_change.p4_path, None, "same spelling as requested");

        let mut last_change = thrift::CommitPathLastChange::default();
        set_p4_fields(
            &mut last_change,
            7,
            P4ChangelistId::new(41002),
            &path,
            "dir/file.txt",
        );
        assert_eq!(last_change.p4_path.as_deref(), Some("Dir/File.txt"));
    }
}
