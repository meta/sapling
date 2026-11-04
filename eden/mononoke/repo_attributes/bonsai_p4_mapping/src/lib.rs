/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod caching;
mod sql;

use ::sql_ext::Transaction;
use anyhow::Error;
use async_trait::async_trait;
use context::CoreContext;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::P4ChangelistId;
use mononoke_types::RepositoryId;

pub use crate::caching::CachingBonsaiP4Mapping;
pub use crate::sql::SqlBonsaiP4Mapping;
pub use crate::sql::SqlBonsaiP4MappingBuilder;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BonsaiP4MappingEntry {
    pub bcs_id: ChangesetId,
    pub p4_changelist_id: P4ChangelistId,
}

impl BonsaiP4MappingEntry {
    pub fn new(bcs_id: ChangesetId, p4_changelist_id: P4ChangelistId) -> Self {
        BonsaiP4MappingEntry {
            bcs_id,
            p4_changelist_id,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AddP4MappingErrorKind {
    #[error("{conflicting} of {total} p4 mapping entries conflict with existing ones")]
    Conflict { conflicting: u64, total: usize },
    #[error("internal error while inserting p4 mapping entries")]
    InternalError(#[from] Error),
}

pub enum BonsaisOrP4ChangelistIds {
    Bonsai(Vec<ChangesetId>),
    P4ChangelistId(Vec<P4ChangelistId>),
}

impl BonsaisOrP4ChangelistIds {
    pub fn is_empty(&self) -> bool {
        match self {
            BonsaisOrP4ChangelistIds::Bonsai(v) => v.is_empty(),
            BonsaisOrP4ChangelistIds::P4ChangelistId(v) => v.is_empty(),
        }
    }
}

impl From<ChangesetId> for BonsaisOrP4ChangelistIds {
    fn from(cs_id: ChangesetId) -> Self {
        BonsaisOrP4ChangelistIds::Bonsai(vec![cs_id])
    }
}

impl From<Vec<ChangesetId>> for BonsaisOrP4ChangelistIds {
    fn from(cs_ids: Vec<ChangesetId>) -> Self {
        BonsaisOrP4ChangelistIds::Bonsai(cs_ids)
    }
}

impl From<P4ChangelistId> for BonsaisOrP4ChangelistIds {
    fn from(cl_id: P4ChangelistId) -> Self {
        BonsaisOrP4ChangelistIds::P4ChangelistId(vec![cl_id])
    }
}

impl From<Vec<P4ChangelistId>> for BonsaisOrP4ChangelistIds {
    fn from(cl_ids: Vec<P4ChangelistId>) -> Self {
        BonsaisOrP4ChangelistIds::P4ChangelistId(cl_ids)
    }
}

/// Mapping between Perforce changelist numbers and the Bonsai changesets they
/// were mirrored into.
///
/// The reverse direction is the reason this exists: given a Bonsai changeset
/// the changelist number can be recovered from the changeset itself, but given
/// a changelist number there is no way to reach a `ChangesetId` without
/// scanning the repository.
///
/// Only *submitted* changelists belong here. Pending and shelved changelists
/// also hold numbers, so "has a changelist number" is not the same as "is in
/// history".
#[facet::facet]
#[async_trait]
pub trait BonsaiP4Mapping: Send + Sync {
    fn repo_id(&self) -> RepositoryId;

    /// Records the given mappings. Errors if any entry conflicts with an existing row on
    /// either side, including re-inserting an identical one: a changelist maps to exactly one
    /// changeset, and a duplicate means the importer did something twice. Unlike svnrev, whose
    /// re-runnable backfill relies on ignoring duplicates, callers here check what is already
    /// mapped before importing.
    async fn bulk_import(
        &self,
        ctx: &CoreContext,
        entries: &[BonsaiP4MappingEntry],
    ) -> Result<(), Error>;

    /// `bulk_import` as part of `transaction`, so the mapping commits or rolls back with
    /// whatever else the transaction writes, such as a bookmark move.
    async fn bulk_import_in_transaction(
        &self,
        ctx: &CoreContext,
        entries: &[BonsaiP4MappingEntry],
        transaction: Transaction,
    ) -> Result<Transaction, AddP4MappingErrorKind>;

    async fn get(
        &self,
        ctx: &CoreContext,
        field: BonsaisOrP4ChangelistIds,
    ) -> Result<Vec<BonsaiP4MappingEntry>, Error>;

    /// The highest mapped changelist, or `None` if nothing is mapped. Read from the primary,
    /// uncached, so a just-committed import is seen.
    async fn get_max(&self, ctx: &CoreContext) -> Result<Option<P4ChangelistId>, Error>;

    async fn get_p4_changelist_id_from_bonsai(
        &self,
        ctx: &CoreContext,
        bcs_id: ChangesetId,
    ) -> Result<Option<P4ChangelistId>, Error> {
        let result = self
            .get(ctx, BonsaisOrP4ChangelistIds::Bonsai(vec![bcs_id]))
            .await?;
        Ok(result
            .into_iter()
            .next()
            .map(|entry| entry.p4_changelist_id))
    }

    async fn get_bonsai_from_p4_changelist_id(
        &self,
        ctx: &CoreContext,
        p4_changelist_id: P4ChangelistId,
    ) -> Result<Option<ChangesetId>, Error> {
        let result = self
            .get(
                ctx,
                BonsaisOrP4ChangelistIds::P4ChangelistId(vec![p4_changelist_id]),
            )
            .await?;
        Ok(result.into_iter().next().map(|entry| entry.bcs_id))
    }

    /// Maps each changeset to the changelist recorded in its extras. Fails the whole batch,
    /// importing nothing, if any changeset doesn't carry a valid changelist: in a Perforce
    /// mirror that means something is wrong, and skipping it would leave a commit that can't
    /// be found by its changelist.
    async fn bulk_import_from_bonsai(
        &self,
        ctx: &CoreContext,
        changesets: &[BonsaiChangeset],
    ) -> anyhow::Result<()> {
        let entries = changesets
            .iter()
            .map(|bcs| {
                Ok(BonsaiP4MappingEntry::new(
                    bcs.get_changeset_id(),
                    P4ChangelistId::from_bcs(bcs)?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.bulk_import(ctx, &entries).await
    }
}
