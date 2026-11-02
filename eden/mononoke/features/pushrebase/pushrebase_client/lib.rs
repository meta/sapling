/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod batched;
#[cfg(fbcode_build)]
mod facebook;
mod hybrid;
mod land_service;
mod local;
#[cfg(not(fbcode_build))]
mod oss;

use std::collections::HashMap;

pub use batched::BatchedPushrebaseClient;
use bookmarks_movement::BookmarkKindRestrictions;
use bookmarks_movement::BookmarkMovementError;
use bookmarks_types::BookmarkKey;
use bytes::Bytes;
use context::CoreContext;
#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(fbcode_build)]
pub use facebook::override_certificate_paths as land_service_override_certificate_paths;
use hooks::CrossRepoPushSource;
pub use hybrid::normal_pushrebase;
pub use land_service::LandServicePushrebaseClient;
pub use local::LocalPushrebaseClient;
use mononoke_types::BonsaiChangeset;
#[cfg(not(fbcode_build))]
use oss as platform;
use pushrebase::PushrebaseOutcome;
use repo_authorization::AuthorizationContext;

#[async_trait::async_trait]
/// This trait provides an abstraction for pushrebase, which can be used to allow
/// pushrebase to happen remotely.
pub trait PushrebaseClient: Sync + Send {
    /// Pushrebase the given changesets to the given bookmark.
    ///
    /// Per-request control over `pushrebase_enable_merge_resolution` is
    /// expressed via the `MERGE_RESOLUTION_OVERRIDE` pushvar; parsing
    /// happens at the terminal pushrebase implementation.
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
    ) -> Result<PushrebaseOutcome, BookmarkMovementError>;
}
