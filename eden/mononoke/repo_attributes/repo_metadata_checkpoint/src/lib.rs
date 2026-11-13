/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod sql;

use anyhow::Result;
use async_trait::async_trait;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;

pub use crate::sql::SqlRepoMetadataCheckpoint;
pub use crate::sql::SqlRepoMetadataCheckpointBuilder;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RepoMetadataCheckpointEntry {
    pub changeset_id: ChangesetId,
    pub bookmark_name: String,
    pub last_updated_timestamp: Timestamp,
}

impl RepoMetadataCheckpointEntry {
    pub fn new(
        changeset_id: ChangesetId,
        bookmark_name: String,
        last_updated_timestamp: Timestamp,
    ) -> Self {
        RepoMetadataCheckpointEntry {
            changeset_id,
            bookmark_name,
            last_updated_timestamp,
        }
    }
}

#[facet::facet]
#[async_trait]
/// Facet trait representing repo metadata info for a repo
pub trait RepoMetadataCheckpoint: Send + Sync {
    /// The repository for which this entry has been created
    fn repo_id(&self) -> RepositoryId;

    /// Fetch all the metadata info entries for the given repo
    async fn get_all_entries(&self) -> Result<Vec<RepoMetadataCheckpointEntry>>;

    /// Fetch the repo metadata entries corresponding to the input bookmark name
    /// for the given repo
    async fn get_entry(&self, bookmark_name: String)
    -> Result<Option<RepoMetadataCheckpointEntry>>;

    /// Add new or update existing repo metadata entries for the given repo
    async fn add_or_update_entries(&self, entries: Vec<RepoMetadataCheckpointEntry>) -> Result<()>;

    /// Get the fallback bookmark last chosen for this repo, if any.
    async fn get_fallback_bookmark(&self) -> Result<Option<String>>;

    /// Record the fallback bookmark chosen for this repo. Does nothing until a
    /// full mode run has been recorded for the repo.
    async fn set_fallback_bookmark(&self, bookmark_name: String) -> Result<()>;

    /// Get the timestamp of the last successful full mode run for this repo.
    /// Returns None if no full run has ever completed.
    async fn get_last_full_run_timestamp(&self) -> Result<Option<Timestamp>>;

    /// Atomically record a successful full run and any supplied fallback change.
    /// When no change is supplied, preserve the currently stored choice.
    async fn record_full_run(
        &self,
        timestamp: Timestamp,
        fallback_bookmark: Option<String>,
    ) -> Result<()>;
}

/// Check whether a full run should be skipped based on the last run timestamp.
/// Returns true if the last run was within `interval_secs` of now.
pub async fn should_skip_full_run(
    checkpoint: &dyn RepoMetadataCheckpoint,
    interval_secs: i64,
) -> Result<bool> {
    let last_run = checkpoint.get_last_full_run_timestamp().await?;
    match last_run {
        None => Ok(false),
        Some(ts) => Ok(ts.since_seconds() < interval_secs),
    }
}
