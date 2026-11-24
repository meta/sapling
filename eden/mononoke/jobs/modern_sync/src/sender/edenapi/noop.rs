/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use async_trait::async_trait;
use edenapi_types::AnyFileContentId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::blobs::HgBlobChangeset;
use minibytes::Bytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;

use crate::sender::edenapi::BookmarkMove;
use crate::sender::edenapi::EdenapiSender;

#[derive(Default)]
pub struct NoopEdenapiSender {}

#[async_trait]
impl EdenapiSender for NoopEdenapiSender {
    async fn upload_contents(&self, _contents: Vec<(AnyFileContentId, Bytes)>) -> Result<()> {
        Ok(())
    }

    async fn upload_trees(&self, _trees: Vec<HgManifestId>) -> Result<()> {
        Ok(())
    }

    async fn upload_filenodes(&self, _fn_ids: Vec<HgFileNodeId>) -> Result<()> {
        Ok(())
    }

    async fn set_bookmark(&self, _bookmark: String, _moves: Vec<BookmarkMove>) -> Result<()> {
        Ok(())
    }

    async fn upload_identical_changeset(
        &self,
        _css: Vec<(HgBlobChangeset, BonsaiChangeset)>,
    ) -> Result<()> {
        return Ok(());
    }

    async fn filter_existing_commits(
        &self,
        ids: Vec<(HgChangesetId, ChangesetId)>,
    ) -> Result<Vec<ChangesetId>> {
        Ok(ids.iter().map(|id| id.1).collect())
    }

    async fn read_bookmark(&self, _bookmark: String) -> Result<Option<HgChangesetId>> {
        Ok(None)
    }

    async fn filter_existing_contents(&self, ids: Vec<ContentId>) -> Result<Vec<ContentId>> {
        Ok(ids)
    }
}
