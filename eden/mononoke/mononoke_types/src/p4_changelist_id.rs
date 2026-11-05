/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::str;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use sql::mysql;

use crate::BonsaiChangeset;

/// Extra marking a changeset as imported from Perforce, with [`P4_SOURCE_VALUE`].
///
/// TODO(T291708646): confirm the source key and value with the team. Whatever they are,
/// the importer writes them through [`P4ChangelistId::to_hg_extras`] and
/// [`P4ChangelistId::from_bcs`] reads them back, so the two cannot drift apart.
const P4_SOURCE_EXTRA: &str = "convert_source";
const P4_SOURCE_VALUE: &str = "p4";
/// Extra carrying the changelist number, as a bare decimal.
const P4_CHANGELIST_EXTRA: &str = "convert_revision";

/// A Perforce changelist number.
///
/// Changelist numbers are allocated by the Perforce server from a single
/// counter shared by every depot on that server -- they are not per-depot and
/// not per-branch. A changelist number is therefore only meaningful relative
/// to one Perforce server.
///
/// Numbering starts at 1; there is no changelist 0. The sequence is sparse:
/// abandoned pending changelists and obliterated changelists both leave holes.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(mysql::OptTryFromRowField)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct P4ChangelistId(u64);

impl P4ChangelistId {
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    #[inline]
    pub fn id(&self) -> u64 {
        self.0
    }

    /// The extras a changeset imported from this changelist carries.
    pub fn to_hg_extras(&self) -> [(String, Vec<u8>); 2] {
        [
            (P4_SOURCE_EXTRA.to_string(), P4_SOURCE_VALUE.into()),
            (P4_CHANGELIST_EXTRA.to_string(), self.0.to_string().into()),
        ]
    }

    /// Reads back the changelist written by [`Self::to_hg_extras`]. Errors unless the
    /// changeset is marked as imported from Perforce, as `bonsai_git_mapping` does for
    /// git, so an svn commit (which also uses `convert_revision`) is never misread.
    pub fn from_bcs(bcs: &BonsaiChangeset) -> Result<Self> {
        Self::from_hg_extras(bcs.hg_extra())
            .with_context(|| format!("Bonsai cs {}", bcs.get_changeset_id()))
    }

    /// [`Self::from_bcs`] for a changeset's hg extras from elsewhere, like `ChangesetInfo`.
    pub fn from_hg_extras<'a>(
        extras: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<Self> {
        let (mut source, mut changelist) = (None, None);
        for (key, value) in extras {
            if key == P4_SOURCE_EXTRA {
                source = Some(value);
            }
            if key == P4_CHANGELIST_EXTRA {
                changelist = Some(value);
            }
        }
        ensure!(
            source == Some(P4_SOURCE_VALUE.as_bytes()),
            "not imported from Perforce"
        );
        let changelist = changelist.ok_or_else(|| anyhow!("no p4 changelist"))?;
        let parse = || -> Result<u64> { Ok(str::from_utf8(changelist)?.parse()?) };
        let id = parse().with_context(|| {
            format!(
                "malformed p4 changelist {:?}",
                String::from_utf8_lossy(changelist)
            )
        })?;
        ensure!(id != 0, "p4 changelist 0, which Perforce never allocates");
        ensure!(
            i64::try_from(id).is_ok(),
            "p4 changelist {id} above i64::MAX, which the mapping can't store"
        );
        Ok(Self::new(id))
    }
}

#[cfg(test)]
mod tests;
