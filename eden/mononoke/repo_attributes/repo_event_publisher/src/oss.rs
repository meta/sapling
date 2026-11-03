/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use repo_update_logger::GitContentRefInfo;
use repo_update_logger::PlainBookmarkInfo;
use tokio::sync::broadcast;

use crate::RepoEventPublisher;
use crate::RepoName;

pub struct UnsupportedRepoEventPublisher;

impl RepoEventPublisher for UnsupportedRepoEventPublisher {
    fn subscribe_for_bookmark_updates(
        &self,
        _repo_name: &RepoName,
    ) -> Result<broadcast::Receiver<PlainBookmarkInfo>> {
        anyhow::bail!("Subscription to bookmark updates is not supported in OSS mode");
    }

    fn subscribe_for_tag_updates(
        &self,
        _repo_name: &RepoName,
    ) -> Result<broadcast::Receiver<PlainBookmarkInfo>> {
        anyhow::bail!("Subscription to tag updates is not supported in OSS mode");
    }

    fn subscribe_for_content_refs_updates(
        &self,
        _repo_name: &RepoName,
    ) -> Result<broadcast::Receiver<GitContentRefInfo>> {
        anyhow::bail!("Subscription to content refs updates is not supported in OSS mode");
    }
}
