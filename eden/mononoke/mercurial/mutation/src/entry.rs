/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Read;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use edenapi_types::Extra;
use edenapi_types::HgMutationEntryContent;
use hg_mutation_entry_thrift as thrift;
use mercurial_types::HgChangesetId;
use mercurial_types::HgNodeHash;
#[cfg(test)]
use quickcheck_arbitrary_derive::Arbitrary;
use types::HgId;
use types::mutation::MutationEntry;

/// Record of a Mercurial mutation operation (e.g. amend or rebase).
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
#[derive(bincode::Encode, bincode::Decode)]
#[cfg_attr(test, derive(Arbitrary))]
pub struct HgMutationEntry {
    /// The commit that resulted from the mutation operation.
    successor: HgChangesetId,
    /// The commits that were mutated to create the successor.
    ///
    /// There may be multiple predecessors, e.g. if the commits were folded.
    predecessors: Vec<HgChangesetId>,
    /// Other commits that were created by the mutation operation splitting the predecessors.
    ///
    /// Where a commit is split into two or more commits, the successor will be the final commit,
    /// and this list will contain the other commits.
    split: Vec<HgChangesetId>,
    /// The name of the operation.
    op: String,
    /// The user who performed the mutation operation.  This may differ from the commit author.
    user: String,
    /// The timestamp of the mutation operation.  This may differ from the commit time.
    timestamp: i64,
    /// The timezone offset of the mutation operation.  This may differ from the commit time.
    timezone: i32,
    /// Extra information about this mutation operation.
    extra: Vec<(String, String)>,
}

impl HgMutationEntry {
    pub fn new(
        successor: HgChangesetId,
        predecessors: Vec<HgChangesetId>,
        split: Vec<HgChangesetId>,
        op: String,
        user: String,
        timestamp: i64,
        timezone: i32,
        extra: Vec<(String, String)>,
    ) -> Self {
        Self {
            successor,
            predecessors,
            split,
            op,
            user,
            timestamp,
            timezone,
            extra,
        }
    }

    pub fn deserialize(r: &mut dyn Read) -> Result<Self> {
        HgMutationEntry::try_from(MutationEntry::deserialize(r)?)
    }

    pub fn successor(&self) -> &HgChangesetId {
        &self.successor
    }

    pub fn predecessors(
        &self,
    ) -> impl ExactSizeIterator<Item = &HgChangesetId> + DoubleEndedIterator<Item = &HgChangesetId>
    {
        self.predecessors.iter()
    }

    pub fn split(
        &self,
    ) -> impl ExactSizeIterator<Item = &HgChangesetId> + DoubleEndedIterator<Item = &HgChangesetId>
    {
        self.split.iter()
    }

    pub fn op(&self) -> &str {
        &self.op
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    pub fn timezone(&self) -> i32 {
        self.timezone
    }

    pub fn extra(&self) -> impl Iterator<Item = (&str, &str)> {
        self.extra.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Returns `extra` encoded with JSON.
    pub fn extra_json(&self) -> Result<String> {
        let extra = self.extra().collect::<Vec<_>>();
        Ok(serde_json::to_string(&extra)?)
    }

    /// Add the next predecessor to the entry.
    pub(crate) fn add_predecessor(&mut self, index: u64, pred: HgChangesetId) -> Result<()> {
        // This method is used when progressively loading entries from the
        // database. Each predecessor is received in a separate row, and we may
        // receive each predecessor multiple times.  They should always be
        // received in order, so only extend the list of predecessors if the
        // index of the new predecessor matches the expected index of the next
        // predecessor.
        let expected_index = self.predecessors.len() as u64;
        if index > expected_index {
            // We have received a predecessor past the end of the current
            // predecessor list.  This probably means the predecessor table is
            // missing a row.
            return Err(anyhow!(
                "Unexpected out-of-order predecessor {pred}, expected index {expected_index}"
            ));
        }
        if index == expected_index {
            self.predecessors.push(pred);
        }
        Ok(())
    }

    /// Add the next split to the entry.
    pub(crate) fn add_split(&mut self, index: u64, split: HgChangesetId) -> Result<()> {
        // This method is used when progressively loading entries from the
        // database. Each split successor is received in a separate row, and we
        // may receive each split successor multiple times.  They should always
        // be received in order, so only extend the list of split successors if
        // the index of the new split successor matches the expected index of
        // the next split successor.
        let expected_index = self.split.len() as u64;
        if index > expected_index {
            // We have received a split successor past the end of the current
            // split successor list.  This probably means the split table is
            // missing a row.
            return Err(anyhow!(
                "Unexpected out-of-order split successor {split}, expected index {expected_index}"
            ));
        }
        if index == expected_index {
            self.split.push(split);
        }
        Ok(())
    }

    pub(crate) fn from_thrift(entry: thrift::HgMutationEntry) -> Result<Self> {
        let preds = entry
            .predecessors
            .into_iter()
            .map(HgChangesetId::from_thrift)
            .collect::<Result<Vec<HgChangesetId>>>()?;
        let split = entry
            .split
            .into_iter()
            .map(HgChangesetId::from_thrift)
            .collect::<Result<Vec<HgChangesetId>>>()?;
        let extra = entry
            .extra
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect::<Vec<(String, String)>>();
        Ok(HgMutationEntry::new(
            HgChangesetId::from_thrift(entry.successor)?,
            preds,
            split,
            entry.op,
            entry.user,
            entry.timestamp,
            entry.timezone,
            extra,
        ))
    }

    pub(crate) fn into_thrift(self) -> thrift::HgMutationEntry {
        thrift::HgMutationEntry {
            successor: HgChangesetId::into_thrift(self.successor),
            predecessors: self
                .predecessors
                .into_iter()
                .map(HgChangesetId::into_thrift)
                .collect(),
            split: self
                .split
                .into_iter()
                .map(HgChangesetId::into_thrift)
                .collect(),
            op: self.op,
            user: self.user,
            timestamp: self.timestamp,
            timezone: self.timezone,
            extra: self
                .extra
                .into_iter()
                .map(|(key, value)| thrift::ExtraProperty { key, value })
                .collect(),
        }
    }
}

// Conversion from client mutation entry
impl TryFrom<MutationEntry> for HgMutationEntry {
    type Error = Error;

    fn try_from(entry: MutationEntry) -> Result<HgMutationEntry> {
        let entry = HgMutationEntry {
            successor: HgChangesetId::new(HgNodeHash::from(entry.succ)),
            predecessors: entry
                .preds
                .into_iter()
                .map(HgNodeHash::from)
                .map(HgChangesetId::new)
                .collect(),
            split: entry
                .split
                .into_iter()
                .map(HgNodeHash::from)
                .map(HgChangesetId::new)
                .collect(),
            op: entry.op,
            user: entry.user,
            timestamp: entry.time,
            timezone: entry.tz,
            extra: entry
                .extra
                .into_iter()
                .map(|(key, value)| -> Result<(String, String), Error> {
                    Ok((
                        String::from_utf8(key.into())?,
                        String::from_utf8(value.into())?,
                    ))
                })
                .collect::<Result<_>>()?,
        };
        Ok(entry)
    }
}

impl From<HgMutationEntry> for MutationEntry {
    fn from(m: HgMutationEntry) -> MutationEntry {
        MutationEntry {
            succ: m.successor.into_nodehash().into(),
            preds: m
                .predecessors
                .into_iter()
                .map(HgChangesetId::into_nodehash)
                .map(HgId::from)
                .collect(),
            split: m
                .split
                .into_iter()
                .map(HgChangesetId::into_nodehash)
                .map(HgId::from)
                .collect(),
            op: m.op,
            user: m.user,
            time: m.timestamp,
            tz: m.timezone,
            extra: m
                .extra
                .into_iter()
                .map(|(key, value)| {
                    (
                        key.into_bytes().into_boxed_slice(),
                        value.into_bytes().into_boxed_slice(),
                    )
                })
                .collect(),
        }
    }
}

impl TryFrom<HgMutationEntryContent> for HgMutationEntry {
    type Error = Error;
    fn try_from(mutation: HgMutationEntryContent) -> Result<Self> {
        let successor = HgChangesetId::new(HgNodeHash::from(mutation.successor));
        let predecessors = mutation
            .predecessors
            .into_iter()
            .map(HgNodeHash::from)
            .map(HgChangesetId::new)
            .collect::<Vec<_>>();
        let split = mutation
            .split
            .into_iter()
            .map(HgNodeHash::from)
            .map(HgChangesetId::new)
            .collect::<Vec<_>>();
        let op = mutation.op;
        let user = std::str::from_utf8(&mutation.user)?.to_string();
        let timestamp = mutation.time;
        let timezone = mutation.tz;
        let extra = mutation
            .extras
            .into_iter()
            .map(|extra| {
                Ok((
                    std::str::from_utf8(&extra.key)?.to_string(),
                    std::str::from_utf8(&extra.value)?.to_string(),
                ))
            })
            .collect::<Result<_, Error>>()?;

        Ok(HgMutationEntry::new(
            successor,
            predecessors,
            split,
            op,
            user,
            timestamp,
            timezone,
            extra,
        ))
    }
}
impl From<HgMutationEntry> for HgMutationEntryContent {
    fn from(mutation: HgMutationEntry) -> Self {
        let successor = mutation.successor.into();
        let predecessors = mutation
            .predecessors
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let split = mutation
            .split
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let op = mutation.op;
        let user = mutation.user.into_bytes();
        let time = mutation.timestamp;
        let tz = mutation.timezone;
        let extras = mutation
            .extra
            .into_iter()
            .map(|(key, value)| Extra {
                key: key.into_bytes(),
                value: value.into_bytes(),
            })
            .collect();

        Self {
            successor,
            predecessors,
            split,
            op,
            user,
            time,
            tz,
            extras,
        }
    }
}
