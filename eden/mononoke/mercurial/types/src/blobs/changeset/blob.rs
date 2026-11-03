/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Display;
use std::io::Write;

use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use bytes::Bytes;
use context::CoreContext;
use futures_watchdog::WatchdogExt;
use mononoke_types::DateTime;

use super::revlog::Extra;
use super::revlog::RevlogChangeset;
use super::revlog::serialize_extras;
use crate::HgBlobNode;
use crate::HgChangesetEnvelopeMut;
use crate::HgNodeHash;
use crate::HgParents;
use crate::NonRootMPath;
use crate::nodehash::HgChangesetId;
use crate::nodehash::HgManifestId;
use crate::subtree::HgSubtreeChanges;

const STEP_PARENTS_METADATA_KEY: &[u8] = b"stepparents";
const COMMITTER_METADATA_KEY: &[u8] = b"committer";
const SUBTREE_METADATA_KEY: &[u8] = b"subtree";

pub struct ChangesetMetadata {
    pub user: String,
    pub time: DateTime,
    pub extra: BTreeMap<Bytes, Bytes>,
    pub message: String,
}

impl ChangesetMetadata {
    pub fn record_step_parents(&mut self, step_parents: impl Iterator<Item = HgChangesetId>) {
        let mut meta = Vec::new();

        for (idx, parent) in step_parents.enumerate() {
            if idx > 0 {
                write!(meta, ",").expect("writes to memory don't fail");
            }
            write!(meta, "{parent}").expect("writes to memory don't fail");
        }

        if meta.is_empty() {
            return;
        }

        self.extra
            .insert(STEP_PARENTS_METADATA_KEY.into(), meta.into());
    }

    pub fn record_committer(
        &mut self,
        committer: &str,
        committer_time: &DateTime,
    ) -> Result<(), Error> {
        if self.extra.contains_key(COMMITTER_METADATA_KEY) {
            bail!("committer extra is already set, can't insert another one!");
        }

        // Use the same format as hggit extension - https://fburl.com/diffusion/3ckf76fd
        let value = format!(
            "{} {} {}",
            committer,
            committer_time.timestamp_secs(),
            committer_time.tz_offset_secs()
        );

        self.extra
            .insert(COMMITTER_METADATA_KEY.into(), value.into());

        Ok(())
    }

    pub fn record_subtree_changes(
        &mut self,
        subtree_changes: HgSubtreeChanges,
    ) -> Result<(), Error> {
        let json = subtree_changes.to_json()?;
        self.extra
            .insert(SUBTREE_METADATA_KEY.into(), Bytes::from(json));
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct HgChangesetContent {
    p1: Option<HgNodeHash>,
    p2: Option<HgNodeHash>,
    manifestid: HgManifestId,
    user: Bytes,
    time: DateTime,
    extra: Extra,
    files: Vec<NonRootMPath>,
    message: Bytes,
}

impl HgChangesetContent {
    pub fn new_from_parts(
        // XXX replace parents with p1 and p2
        parents: HgParents,
        manifestid: HgManifestId,
        cs_metadata: ChangesetMetadata,
        files: Vec<NonRootMPath>,
    ) -> Self {
        let (p1, p2) = parents.get_nodes();
        Self {
            p1,
            p2,
            manifestid,
            user: cs_metadata.user.into(),
            time: cs_metadata.time,
            extra: Extra::new(cs_metadata.extra),
            files,
            message: cs_metadata.message.into(),
        }
    }

    pub fn from_revlogcs(revlogcs: RevlogChangeset) -> Self {
        Self {
            p1: revlogcs.p1,
            p2: revlogcs.p2,
            manifestid: revlogcs.manifestid,
            user: revlogcs.user,
            time: revlogcs.time,
            extra: revlogcs.extra,
            files: revlogcs.files,
            message: revlogcs.message,
        }
    }

    pub fn compute_hash(&self) -> Result<HgChangesetId> {
        let mut v = Vec::new();

        self.generate(&mut v)?;
        let blobnode = HgBlobNode::new(Bytes::from(v), self.p1(), self.p2());

        let nodeid = blobnode.nodeid();
        Ok(HgChangesetId::new(nodeid))
    }

    /// Generate a serialized changeset. This is the counterpart to parse, and generates
    /// in the same format as Mercurial. It should be bit-for-bit identical in fact.
    fn generate<W: Write>(&self, out: &mut W) -> Result<()> {
        write!(out, "{}\n", self.manifestid.into_nodehash())?;
        out.write_all(&self.user)?;
        out.write_all(b"\n")?;
        write!(
            out,
            "{} {}",
            self.time.timestamp_secs(),
            self.time.tz_offset_secs()
        )?;

        if !self.extra.is_empty() {
            write!(out, " ")?;
            serialize_extras(&self.extra, out)?;
        }

        write!(out, "\n")?;
        for f in &self.files {
            f.generate(out)?;
            out.write_all(b"\n")?;
        }
        write!(out, "\n")?;
        out.write_all(&self.message)?;

        Ok(())
    }

    #[inline]
    pub fn p1(&self) -> Option<HgNodeHash> {
        self.p1
    }

    #[inline]
    pub fn p2(&self) -> Option<HgNodeHash> {
        self.p2
    }
}

#[derive(Debug, Clone)]
pub struct HgBlobChangeset {
    changesetid: HgChangesetId, // redundant - can be computed from revlogcs?
    content: HgChangesetContent,
}

impl HgBlobChangeset {
    pub fn new(content: HgChangesetContent) -> Result<Self> {
        Ok(Self::new_with_id(content.compute_hash()?, content))
    }

    pub fn new_with_id(changesetid: HgChangesetId, content: HgChangesetContent) -> Self {
        Self {
            changesetid,
            content,
        }
    }

    pub fn get_changeset_id(&self) -> HgChangesetId {
        self.changesetid
    }

    pub async fn load<'a, B: KeyedBlobstore>(
        ctx: &'a CoreContext,
        blobstore: &'a B,
        changesetid: HgChangesetId,
    ) -> Result<Option<Self>> {
        let got = RevlogChangeset::load(ctx, blobstore, changesetid)
            .watched()
            .with_max_poll(blobstore::BLOBSTORE_MAX_POLL_TIME_MS)
            .await?;
        Ok(got.map(|revlogcs| {
            HgBlobChangeset::new_with_id(changesetid, HgChangesetContent::from_revlogcs(revlogcs))
        }))
    }

    pub async fn save<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<()> {
        let key = self.changesetid.blobstore_key();

        let contents = {
            let mut v = Vec::new();

            self.content.generate(&mut v).map(|()| Bytes::from(v))?
        };

        let envelope = HgChangesetEnvelopeMut {
            node_id: self.changesetid,
            p1: self.content.p1().map(HgChangesetId::new),
            p2: self.content.p2().map(HgChangesetId::new),
            contents,
        };
        let envelope = envelope.freeze();
        let blob = envelope.into_blob();
        blobstore.put(ctx, key, blob.into()).await
    }

    #[inline]
    pub fn p1(&self) -> Option<HgNodeHash> {
        self.content.p1()
    }

    #[inline]
    pub fn p2(&self) -> Option<HgNodeHash> {
        self.content.p2()
    }

    pub fn manifestid(&self) -> HgManifestId {
        self.content.manifestid
    }

    pub fn user(&self) -> &[u8] {
        &self.content.user
    }

    pub fn extra(&self) -> &BTreeMap<Bytes, Bytes> {
        self.content.extra.as_ref()
    }

    pub fn message(&self) -> &[u8] {
        &self.content.message
    }

    pub fn files(&self) -> &[NonRootMPath] {
        &self.content.files
    }

    pub fn time(&self) -> &DateTime {
        &self.content.time
    }

    pub fn parents(&self) -> HgParents {
        // XXX Change this to return p1 and p2 directly.
        HgParents::new(self.content.p1(), self.content.p2())
    }

    pub fn step_parents(&self) -> Result<Vec<HgNodeHash>> {
        let mut ret = vec![];

        if let Some(step_parents) = self.extra().get(STEP_PARENTS_METADATA_KEY) {
            let step_parents = std::str::from_utf8(step_parents)?;
            for csid in step_parents.split(',') {
                let csid = csid.parse()?;
                ret.push(csid);
            }
        }

        Ok(ret)
    }
}

#[async_trait]
impl Loadable for HgChangesetId {
    type Value = HgBlobChangeset;

    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        let csid = *self;
        let value = HgBlobChangeset::load(ctx, blobstore, csid).await?;
        value.ok_or_else(|| LoadableError::Missing(csid.blobstore_key()))
    }
}

impl Display for HgBlobChangeset {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let message = self.message();
        let title_end = message
            .iter()
            .enumerate()
            .find(|&(_, &c)| c == b'\n')
            .map_or(message.len(), |(i, _)| i);

        write!(
            f,
            "changeset: {}\nauthor: {}\ndate: {}\nsummary: {}\n",
            self.changesetid,
            String::from_utf8_lossy(self.user()),
            self.time().as_chrono().to_rfc2822(),
            String::from_utf8_lossy(&self.message()[0..title_end])
        )
    }
}
