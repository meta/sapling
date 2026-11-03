/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Repo Event Publisher.
//!
//! Responsible for publishing repo events (e.g. bookmark updates, tag updates, etc.)
//! and allowing interested parties to subscribe to them.

#![cfg_attr(fbcode_build, feature(trait_alias))]

#[cfg(fbcode_build)]
mod facebook;
#[cfg(fbcode_build)]
mod from_scuba_json;
#[cfg(fbcode_build)]
mod notification_filter;
#[cfg(not(fbcode_build))]
mod oss;
#[cfg(fbcode_build)]
mod repo_name_provider;

use anyhow::Result;
#[cfg(fbcode_build)]
pub use facebook::scribe_listener::ScribeListener;
#[cfg(fbcode_build)]
pub use facebook::scribe_repo_event_publisher::ScribeRepoEventPublisher;
#[cfg(fbcode_build)]
pub use mononoke_bookmark_event_notification::BookmarkCategory;
#[cfg(fbcode_build)]
pub use mononoke_bookmark_event_notification::BookmarkEventNotification;
#[cfg(fbcode_build)]
pub use notification_filter::AllBookmarksFilter;
#[cfg(not(fbcode_build))]
pub use oss::UnsupportedRepoEventPublisher;
use repo_update_logger::GitContentRefInfo;
use repo_update_logger::PlainBookmarkInfo;
use tokio::sync::broadcast;

/// The name of the repo.
pub(crate) type RepoName = String;

/// The core Repo Event Publisher facet.
#[facet::facet]
pub trait RepoEventPublisher {
    /// Subscribe to bookmark create/update/delete notifications for the repo.
    fn subscribe_for_bookmark_updates(
        &self,
        repo_name: &RepoName,
    ) -> Result<broadcast::Receiver<PlainBookmarkInfo>>;

    /// Subscribe to EventRouter bookmark notifications for the repo.
    #[cfg(fbcode_build)]
    fn subscribe_for_bookmark_event_notifications(
        &self,
        repo_name: &RepoName,
    ) -> Result<broadcast::Receiver<BookmarkEventNotification>>;

    /// Subscribe to tag create/update/delete notifications for the repo.
    fn subscribe_for_tag_updates(
        &self,
        repo_name: &RepoName,
    ) -> Result<broadcast::Receiver<PlainBookmarkInfo>>;

    /// Subscribe to git content ref create/update/delete notifications for the repo.
    fn subscribe_for_content_refs_updates(
        &self,
        repo_name: &RepoName,
    ) -> Result<broadcast::Receiver<GitContentRefInfo>>;
}
