/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! A store for Mercurial mutation information.
//!
//! This allows mutation information (i.e., which commits where amended or rebased) to be exchanged
//! between Mercurial clients via Mononoke.
//!
//! See <https://fburl.com/m2y3nr5c> for more details.

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use async_trait::async_trait;
use context::CoreContext;
use mercurial_types::HgChangesetId;
use mononoke_types::RepositoryId;

mod builder;
mod caching;
mod entry;
mod store;

pub use crate::builder::SqlHgMutationStoreBuilder;
pub use crate::caching::CachedHgMutationStore;
pub use crate::entry::HgMutationEntry;
pub use crate::store::SqlHgMutationStore;

#[facet::facet]
#[async_trait]
pub trait HgMutationStore: Send + Sync {
    /// Add new entries to the mutation store.
    ///
    /// Adds mutation information for `new_changeset_ids` using the given `entries`.
    async fn add_entries(
        &self,
        ctx: &CoreContext,
        new_changeset_ids: HashSet<HgChangesetId>,
        entries: Vec<HgMutationEntry>,
    ) -> Result<()>;

    /// Get all predecessor information for the given changeset ids.
    ///
    /// Returns all entries that describe the mutation history of the commits.
    async fn all_predecessors(
        &self,
        ctx: &CoreContext,
        changeset_ids: HashSet<HgChangesetId>,
    ) -> Result<Vec<HgMutationEntry>> {
        let entries_by_changeset = self
            .all_predecessors_by_changeset(ctx, changeset_ids)
            .await?;
        Ok(entries_by_changeset
            .into_values()
            .flatten()
            // Collect into a hashset since the preds for different
            // successors might overlap due to fold and split.
            .collect::<HashSet<_>>()
            .into_iter()
            .collect())
    }

    /// Get all predecessor information for the given changeset id, keyed by
    /// the successor changeset id.
    ///
    /// Returns all entries that describe the mutation history of the commits.
    /// keyed by the successor changeset ids.
    async fn all_predecessors_by_changeset(
        &self,
        ctx: &CoreContext,
        changeset_ids: HashSet<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, Vec<HgMutationEntry>>>;

    /// Get the repository for which the mutation history is being added
    /// and retrieved.
    fn repo_id(&self) -> RepositoryId;
}
