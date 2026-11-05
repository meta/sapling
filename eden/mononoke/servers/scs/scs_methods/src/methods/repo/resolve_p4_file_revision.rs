/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use bookmarks::BookmarkKey;
use context::CoreContext;
use mononoke_api::BookmarkFreshness;
use mononoke_api::MononokeError;
use mononoke_types::path::MPath;
use source_control as thrift;

use crate::commit_id::map_commit_identity;
use crate::source_control_impl::SourceControlServiceImpl;

/// The bookmark searched when the request names none.
const DEFAULT_BOOKMARK: &str = "master";

/// How many changes to a path one request reads before giving up. Each costs a
/// `ChangesetInfo` fetch; a `(path, revision)` index will make the walk unnecessary.
const MAX_COMMITS_WALKED: usize = 10_000;

/// The revision a request asks for, if it is one Perforce could have allocated.
pub(crate) fn requested_revision(revision: i64) -> Result<u32, scs_errors::ServiceError> {
    match u32::try_from(revision) {
        Ok(revision) if (1..=i32::MAX as u32).contains(&revision) => Ok(revision),
        _ => Err(scs_errors::invalid_request(format!(
            "revision must be between 1 and {}, not {revision}",
            i32::MAX
        ))
        .into()),
    }
}

impl SourceControlServiceImpl {
    /// Find the commit a Perforce revision of a path was imported as, searching the
    /// path's history back from a bookmark.
    pub(crate) async fn repo_resolve_p4_file_revision(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoResolveP4FileRevisionParams,
    ) -> Result<thrift::RepoResolveP4FileRevisionResponse, scs_errors::ServiceError> {
        let revision = requested_revision(params.revision)?;
        let repo = self.repo(ctx, &repo).await?;
        if repo.config().p4_source.is_none() {
            return Err(scs_errors::invalid_request(format!(
                "repo {} is not a Perforce mirror",
                repo.name()
            ))
            .into());
        }
        MPath::try_from(params.path.as_str())
            .map_err(|error| MononokeError::InvalidRequest(error.to_string()))?
            .into_optional_non_root_path()
            .ok_or_else(|| scs_errors::invalid_request("path must not be the repo root"))?;

        let bookmark = params.bookmark.as_deref().unwrap_or(DEFAULT_BOOKMARK);
        let bookmark_key = BookmarkKey::new(bookmark).map_err(Into::<MononokeError>::into)?;
        let head = repo
            .resolve_bookmark(&bookmark_key, BookmarkFreshness::MaybeStale)
            .await?
            .ok_or_else(|| {
                scs_errors::invalid_request(format!(
                    "bookmark {bookmark} does not exist in repo {}",
                    repo.name()
                ))
            })?;

        let search = head
            .path_with_history(&params.path)
            .await?
            .find_p4_revision(revision, MAX_COMMITS_WALKED)
            .await?;

        let mut response = thrift::RepoResolveP4FileRevisionResponse {
            head_revision: search.head_revision.map(i64::from),
            head_exists: search.head_exists,
            oldest_revision_walked: search.oldest_revision_walked.map(i64::from),
            commits_walked: search.commits_walked as i64,
            ..Default::default()
        };
        if let Some(found) = search.found {
            response.commit =
                Some(map_commit_identity(&found.changeset, &params.identity_schemes).await?);
            response.changelist = Some(found.changelist.id() as i64);
            response.exists = found.exists;
            response.path = Some(found.path.to_string());
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn test_requested_revision_is_one_perforce_could_allocate() {
        assert_eq!(requested_revision(1).unwrap(), 1);
        assert_eq!(
            requested_revision(i32::MAX as i64).unwrap(),
            i32::MAX as u32
        );
        for revision in [0, -1, i32::MAX as i64 + 1, i64::MAX, i64::MIN] {
            let error = requested_revision(revision).expect_err("out of range");
            assert!(
                matches!(
                    &error,
                    scs_errors::ServiceError::Request(request)
                        if request.kind == thrift::RequestErrorKind::INVALID_REQUEST
                            && request.reason.contains(&revision.to_string())
                ),
                "{revision}: {error:?}"
            );
        }
    }
}
