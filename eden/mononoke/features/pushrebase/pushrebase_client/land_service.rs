/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;

use anyhow::Context;
use bookmarks_movement::BookmarkKindRestrictions;
use bookmarks_movement::BookmarkMovementError;
use bookmarks_movement::PushrebaseOutcome;
use bookmarks_types::BookmarkKey;
use bytes::Bytes;
use context::CoreContext;
use hooks::CrossRepoPushSource;
use hooks::HookRejection;
use hooks::HookRejectionInfo;
use hooks::HookRejectionMessage;
use land_service_if::LandChangesetRequest;
use land_service_if_clients::errors::LandChangesetsError;
use metaconfig_types::RepoConfigRef;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use pushrebase::PushrebaseChangesetPair;
use pushrebase::PushrebaseConflict;
use pushrebase::PushrebaseDistance;
use pushrebase::PushrebaseError;
use pushrebase::PushrebaseRetryNum;
use repo_authorization::AuthorizationContext;
use repo_identity::RepoIdentityRef;

use crate::PushrebaseClient;
use crate::platform;

pub struct LandServicePushrebaseClient {
    repo: String,
    client: platform::LandServiceClient,
}

impl LandServicePushrebaseClient {
    pub async fn from_tier(
        ctx: &CoreContext,
        tier: String,
        repo: &(impl RepoIdentityRef + RepoConfigRef),
    ) -> anyhow::Result<Self> {
        Ok(Self {
            repo: repo.repo_identity().name().to_owned(),
            client: platform::LandServiceClient::from_tier(ctx, tier, repo).await?,
        })
    }

    pub async fn from_host_port(
        ctx: &CoreContext,
        host_port: String,
        repo: &impl RepoIdentityRef,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            repo: repo.repo_identity().name().to_owned(),
            client: platform::LandServiceClient::from_host_port(ctx, host_port)?,
        })
    }
}

fn changeset_id_from_bytes(bonsai: Vec<u8>) -> anyhow::Result<ChangesetId> {
    ChangesetId::from_bytes(bonsai)
}

fn to_usize(val: i64) -> anyhow::Result<usize> {
    val.try_into()
        .context("invalid unsigned int in thrift response")
}

fn convert_conflicts(
    conflicts: Vec<land_service_if::PushrebaseConflicts>,
) -> anyhow::Result<Vec<PushrebaseConflict>> {
    conflicts
        .into_iter()
        .map(|c| {
            Ok(PushrebaseConflict {
                left: MPath::new(c.left)?,
                right: MPath::new(c.right)?,
            })
        })
        .collect()
}

fn convert_rejections(
    rejections: Vec<land_service_if::HookRejection>,
) -> anyhow::Result<Vec<HookRejection>> {
    rejections
        .into_iter()
        .map(|rej| {
            Ok(HookRejection {
                hook_name: rej.hook_name,
                cs_id: ChangesetId::from_bytes(rej.cs_id)?,
                reason: HookRejectionInfo {
                    description: std::borrow::Cow::Owned(rej.reason.description),
                    long_description: rej.reason.long_description,
                    message: rej.reason.message_name.map(|name| HookRejectionMessage {
                        name,
                        params: rej
                            .reason
                            .message_params
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                    }),
                },
            })
        })
        .collect()
}

/// Convert Thrift error to proper BookmarkMovementError
///
/// Only pushrebase conflicts, hook rejections and authorization failures are
/// converted specially, the rest are all converted to a generic `Error`.
fn convert_error(err: LandChangesetsError) -> BookmarkMovementError {
    match err {
        LandChangesetsError::pushrebase_conflicts(exc) => convert_conflicts(exc.conflicts)
            .map_or_else(
                |err| {
                    BookmarkMovementError::Error(anyhow::anyhow!(
                        "Failed to parse paths from conflicts: {}.\nOriginal reason: {}",
                        err,
                        exc.reason
                    ))
                },
                |cs| BookmarkMovementError::PushrebaseError(PushrebaseError::Conflicts(cs)),
            ),
        LandChangesetsError::hook_rejections(exc) => convert_rejections(exc.rejections)
            .map_or_else(
                |err| {
                    BookmarkMovementError::Error(anyhow::anyhow!(
                        "Failed to parse changeset from hook failure: {}.\nOriginal reason: {}",
                        err,
                        exc.reason
                    ))
                },
                BookmarkMovementError::HookFailure,
            ),
        LandChangesetsError::internal_error(exc) => {
            BookmarkMovementError::PushrebaseError(PushrebaseError::Error(exc.into()))
        }
        LandChangesetsError::authorization_error(exc) => {
            BookmarkMovementError::AuthorizationError(anyhow::Error::msg(exc.reason).into())
        }
        err => BookmarkMovementError::Error(err.into()),
    }
}

fn convert_changesets(changesets: &[BonsaiChangeset]) -> BTreeSet<Vec<u8>> {
    changesets
        .iter()
        .map(|cs| cs.get_changeset_id().as_ref().to_vec())
        .collect()
}

#[async_trait::async_trait]
impl PushrebaseClient for LandServicePushrebaseClient {
    async fn pushrebase(
        &self,
        ctx: &CoreContext,
        authz: &AuthorizationContext,
        bookmark: &BookmarkKey,
        changesets: &[BonsaiChangeset],
        pushvars: Option<&HashMap<String, Bytes>>,
        cross_repo_push_source: CrossRepoPushSource,
        bookmark_restrictions: BookmarkKindRestrictions,
        log_new_public_commits_to_scribe: bool,
    ) -> Result<PushrebaseOutcome, BookmarkMovementError> {
        let repo_name = self.repo.to_string();
        let changesets = convert_changesets(changesets);
        let cross_repo_push_source = match cross_repo_push_source {
            CrossRepoPushSource::NativeToThisRepo => {
                land_service_if::CrossRepoPushSource::NATIVE_TO_THIS_REPO
            }
            CrossRepoPushSource::PushRedirected => {
                land_service_if::CrossRepoPushSource::PUSH_REDIRECTED
            }
        };
        let bookmark_restrictions = match bookmark_restrictions {
            BookmarkKindRestrictions::AnyKind => {
                land_service_if::BookmarkKindRestrictions::ANY_KIND
            }
            BookmarkKindRestrictions::OnlyScratch => {
                land_service_if::BookmarkKindRestrictions::ONLY_SCRATCH
            }
            BookmarkKindRestrictions::OnlyPublishing => {
                land_service_if::BookmarkKindRestrictions::ONLY_PUBLISHING
            }
        };

        let params = LandChangesetRequest {
            bookmark: bookmark.to_string(),
            changesets,
            pushvars: pushvars.map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), Vec::from(v.as_ref())))
                    .collect()
            }),
            cross_repo_push_source,
            bookmark_restrictions,
            repo_name,
            log_new_public_commits_to_scribe,
            service_identity: authz.service_identity(),
            // Per-request control travels via the MERGE_RESOLUTION_OVERRIDE
            // pushvar; the typed Thrift field is deprecated.
            merge_resolution_override_deprecated: None,
        };

        let response = self
            .client
            .get_service_client(ctx, &self.repo, bookmark)?
            .land_changesets(&params)
            .await
            .map_err(convert_error)?;
        let pushrebase_outcome = response.pushrebase_outcome;
        let outcome = PushrebaseOutcome {
            old_bookmark_value: pushrebase_outcome
                .old_bookmark_value
                .map(changeset_id_from_bytes)
                .transpose()?,
            head: changeset_id_from_bytes(pushrebase_outcome.head)?,
            rebased_changesets: pushrebase_outcome
                .rebased_changesets
                .into_iter()
                .map(|c| {
                    anyhow::Ok(PushrebaseChangesetPair {
                        id_old: changeset_id_from_bytes(c.old_id)?,
                        id_new: changeset_id_from_bytes(c.new_id)?,
                    })
                })
                .collect::<anyhow::Result<_>>()?,
            pushrebase_distance: PushrebaseDistance(to_usize(
                pushrebase_outcome.pushrebase_distance,
            )?),
            retry_num: PushrebaseRetryNum(to_usize(pushrebase_outcome.retry_num)?),
            log_id: pushrebase_outcome
                .log_id
                .expect("log_id should be set in land service")
                .try_into()
                .expect("log_id should be positive"),
            merge_resolved_paths: None,
            merge_summary: None,
        };

        Ok(outcome)
    }
}
