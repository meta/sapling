/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use bonsai_tag_mapping::BonsaiTagMappingRef;
use bytes::Bytes;
use context::CoreContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use git_types::GitError;
use git_types::fetch_non_blob_git_object;
use mononoke_api::ChangesetId;
use mononoke_api::errors::MononokeError;
use mononoke_types::bonsai_changeset::BonsaiAnnotatedTag;
use mononoke_types::bonsai_changeset::BonsaiAnnotatedTagTarget;
use repo_blobstore::RepoBlobstoreArc;
use scs_errors::ServiceErrorResultExt;
use scs_errors::internal_error;
use scs_errors::invalid_request;
use source_control as thrift;

use crate::source_control_impl::SourceControlServiceImpl;

impl SourceControlServiceImpl {
    /// Upload raw git object to Mononoke data store for back-and-forth translation.
    /// Not to be used for uploading raw file content blobs.
    pub(crate) async fn repo_upload_non_blob_git_object(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoUploadNonBlobGitObjectParams,
    ) -> Result<thrift::RepoUploadNonBlobGitObjectResponse, scs_errors::ServiceError> {
        let repo_ctx = self
            .repo_for_service(ctx, &repo, params.service_identity.clone())
            .await
            .with_context(|| format!("Error in opening repo using specifier {repo:?}"))?;
        // Validate that the request sender has an internal service identity with the right permission.
        repo_ctx
            .authorization_context()
            .require_git_import_operations(repo_ctx.ctx(), repo_ctx.repo())
            .await
            .map_err(MononokeError::from)?;
        // Validate that the bytes correspond to a valid git hash.
        let git_hash = gix_hash::oid::try_from_bytes(&params.git_hash)
            .map_err(|_| GitError::InvalidHash(format!("{:x?}", params.git_hash)))?;
        repo_ctx
            .upload_non_blob_git_object(git_hash, params.raw_content)
            .await?;
        Ok(thrift::RepoUploadNonBlobGitObjectResponse {
            ..Default::default()
        })
    }

    /// Create Mononoke counterpart of git tree object in the form of a bonsai changeset.
    /// The raw git tree object must already be stored in Mononoke stores before invoking
    /// this endpoint.
    pub(crate) async fn create_git_tree(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::CreateGitTreeParams,
    ) -> Result<thrift::CreateGitTreeResponse, scs_errors::ServiceError> {
        let repo_ctx = self
            .repo_for_service(ctx, &repo, params.service_identity.clone())
            .await
            .with_context(|| format!("Error in opening repo using specifier {repo:?}"))?;
        // Validate that the request sender has an internal service identity with the right permission.
        repo_ctx
            .authorization_context()
            .require_git_import_operations(repo_ctx.ctx(), repo_ctx.repo())
            .await
            .map_err(MononokeError::from)?;
        // Validate that the provided byte content constitutes a hash.
        let git_tree_hash = gix_hash::oid::try_from_bytes(&params.git_tree_hash)
            .map_err(|_| GitError::InvalidHash(format!("{:x?}", params.git_tree_hash)))?;
        repo_ctx.create_git_tree(git_tree_hash).await?;
        Ok(thrift::CreateGitTreeResponse {
            ..Default::default()
        })
    }

    /// Create Mononoke counterpart of git tag object in the form of a bonsai changeset.
    /// The raw git tag object must already be stored in Mononoke stores before invoking
    /// this endpoint.
    pub(crate) async fn create_git_tag(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::CreateGitTagParams,
    ) -> Result<thrift::CreateGitTagResponse, scs_errors::ServiceError> {
        let repo_ctx = self
            .repo_for_service(ctx, &repo, params.service_identity.clone())
            .await
            .with_context(|| format!("Error in opening repo using specifier {repo:?}"))?;
        // Validate that the request sender has an internal service identity with the right permission.
        repo_ctx
            .authorization_context()
            .require_git_import_operations(repo_ctx.ctx(), repo_ctx.repo())
            .await
            .map_err(MononokeError::from)?;
        let author_date = params
            .author_date
            .as_ref()
            .map(|date| mononoke_types::DateTime::from_timestamp(date.timestamp, date.tz))
            .transpose()
            .map_err(|err| {
                invalid_request(format!(
                    "Error in creating author date from {:?}. Cause: {:#}",
                    params.author_date, err
                ))
            })?
            .map(|date| date.into());

        let target_changeset_id =
            ChangesetId::from_bytes(&params.target_changeset).map_err(|err| {
                invalid_request(format!(
                    "Error in creating ChangesetId from {:?}. Cause: {:#}",
                    params.target_changeset, err
                ))
            })?;
        let annotated_tag = BonsaiAnnotatedTag {
            target: BonsaiAnnotatedTagTarget::Changeset(target_changeset_id),
            pgp_signature: params.pgp_signature.map(Bytes::from),
        };
        let tag_hash = params
            .tag_hash
            .as_ref()
            .map(|hash| {
                gix_hash::oid::try_from_bytes(hash)
                    .map(|oid| oid.to_owned())
                    .map_err(|err| {
                        invalid_request(format!(
                            "Error in creating Git ObjectId from {hash:?}. Cause: {err:#}"
                        ))
                    })
            })
            .transpose()?;
        let target_is_tag = params.target_is_tag.unwrap_or(false);
        let changeset_context = repo_ctx
            .create_annotated_tag(
                tag_hash,
                params.tag_name,
                params.author,
                author_date,
                params.annotation,
                annotated_tag,
                target_is_tag,
            )
            .await?;
        Ok(thrift::CreateGitTagResponse {
            created_changeset_id: changeset_context.id().as_ref().to_vec(),
            ..Default::default()
        })
    }

    /// Upload packfile base item corresponding to git object to Mononoke data store
    pub(crate) async fn repo_upload_packfile_base_item(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoUploadPackfileBaseItemParams,
    ) -> Result<thrift::RepoUploadPackfileBaseItemResponse, scs_errors::ServiceError> {
        let repo_ctx = self
            .repo_for_service(ctx, &repo, params.service_identity.clone())
            .await
            .with_context(|| format!("Error in opening repo using specifier {repo:?}"))?;
        // Validate that the request sender has an internal service identity with the right permission.
        repo_ctx
            .authorization_context()
            .require_git_import_operations(repo_ctx.ctx(), repo_ctx.repo())
            .await
            .map_err(MononokeError::from)?;
        // Validate that the bytes correspond to a valid git hash.
        let git_hash = gix_hash::oid::try_from_bytes(&params.git_hash)
            .map_err(|_| GitError::InvalidHash(format!("{:x?}", params.git_hash)))?;
        repo_ctx
            .repo_upload_packfile_base_item(git_hash, params.raw_content)
            .await?;
        Ok(thrift::RepoUploadPackfileBaseItemResponse {
            ..Default::default()
        })
    }
    /// Fetch tag metadata for annotated tags in the repo
    pub(crate) async fn repo_tag_info(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoTagInfoRequest,
    ) -> Result<thrift::RepoTagInfoResponse, scs_errors::ServiceError> {
        let repo_ctx = self
            .repo_for_service(ctx, &repo, None)
            .await
            .with_context(|| format!("Error in opening repo using specifier {repo:?}"))?;

        let blobstore = repo_ctx.repo().repo_blobstore_arc();

        // Fetch all the requested tag mappings in one call instead of doing a
        // separate lookup per tag below: repos can have hundreds to thousands
        // of git tags, and per-tag lookups either hit the DB once each or
        // linear-scan the in-memory tag cache once each.
        let tag_names: Vec<String> = params.tag_to_hash_map.keys().cloned().collect();
        let tag_entries_by_name: HashMap<String, bonsai_tag_mapping::BonsaiTagMappingEntry> =
            repo_ctx
                .repo()
                .bonsai_tag_mapping()
                .get_entries_by_tag_names(repo_ctx.ctx(), tag_names)
                .await
                .map_err(|err| {
                    internal_error(format!("Error fetching tag mappings. Cause: {err:#}"))
                })?
                .into_iter()
                .map(|entry| (entry.tag_name.clone(), entry))
                .collect();

        let tag_infos = stream::iter(params.tag_to_hash_map)
            .map(|(tag_name, commit_hash)| {
                let ctx = repo_ctx.ctx().clone();
                let blobstore = blobstore.clone();
                let entry = tag_entries_by_name.get(&tag_name).cloned();
                async move {
                    if let Some(entry) = entry {
                        let tag_hash_oid = entry.tag_hash.to_object_id().map_err(|err| {
                            internal_error(format!(
                                "Error in creating Git ObjectId from tag hash for tag '{tag_name}'. Cause: {err:#}"
                            ))
                        })?;

                        let tag_object =
                            fetch_non_blob_git_object(&ctx, &blobstore, &tag_hash_oid)
                                .await
                                .map_err(|err| {
                                    internal_error(format!(
                                        "Error fetching tag object for tag '{tag_name}'. Cause: {err:#}"
                                    ))
                                })?;

                        let (tagger, message, creation_epoch) = tag_object
                            .with_parsed_as_tag(|tag| {
                                let tagger = tag.tagger().ok().flatten();
                                let tagger_name = tagger.as_ref().map(|t| t.name.to_string());
                                let message = tag.message.to_string();
                                let time = tagger
                                    .as_ref()
                                    .and_then(|t| t.time().ok().map(|gix_time| gix_time.seconds))
                                    .unwrap_or(0);
                                (tagger_name, message, time as i32)
                            })
                            .ok_or_else(|| {
                                internal_error(format!(
                                    "Expected tag object for '{tag_name}' but got a different object type"
                                ))
                            })?;

                        Ok::<_, scs_errors::ServiceError>(thrift::TagInfo {
                            object_id: tag_hash_oid.to_hex().to_string(),
                            tag_name: entry.tag_name,
                            tagger,
                            message,
                            creation_epoch,
                            ..Default::default()
                        })
                    } else {
                        let commit_hash_oid =
                            gix_hash::oid::try_from_bytes(&commit_hash).map_err(|_| {
                                invalid_request(format!(
                                    "Invalid commit hash for tag '{tag_name}': {commit_hash:x?}"
                                ))
                            })?;

                        let commit_object =
                            fetch_non_blob_git_object(&ctx, &blobstore, commit_hash_oid)
                                .await
                                .map_err(|err| {
                                    internal_error(format!(
                                        "Error fetching commit object for tag '{tag_name}'. Cause: {err:#}"
                                    ))
                                })?;

                        let (message, creation_epoch) = commit_object
                            .with_parsed_as_commit(|commit| {
                                let message = commit.message.to_string();
                                let time = commit
                                    .author()
                                    .ok()
                                    .and_then(|author| author.time().ok())
                                    .map_or(0, |t| t.seconds);
                                (message, time as i32)
                            })
                            .ok_or_else(|| {
                                internal_error(format!(
                                    "Expected commit object for tag '{tag_name}' but got a different object type"
                                ))
                            })?;

                        Ok::<_, scs_errors::ServiceError>(thrift::TagInfo {
                            object_id: commit_hash_oid.to_hex().to_string(),
                            tag_name,
                            tagger: None,
                            message,
                            creation_epoch,
                            ..Default::default()
                        })
                    }
                }
            })
            .buffer_unordered(50)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(thrift::RepoTagInfoResponse {
            tag_infos,
            ..Default::default()
        })
    }
}
