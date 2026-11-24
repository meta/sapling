/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Ok;
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

#[derive(Eq, Hash, PartialEq)]
pub enum MethodFilter {
    UploadContents,
    UploadTrees,
    UploadFilenodes,
    SetBookmark,
    UploadIdenticalChangeset,
    FilterExistingCommits,
    ReadBookmark,
    FilterExistingContents,
}

pub struct FilterEdenapiSender {
    inner: Arc<dyn EdenapiSender + Send + Sync>,
    allowed: HashMap<MethodFilter, bool>,
}

impl FilterEdenapiSender {
    pub fn new(
        inner: Arc<dyn EdenapiSender + Send + Sync>,
        allowed: HashMap<MethodFilter, bool>,
    ) -> Self {
        Self { inner, allowed }
    }
}

#[async_trait]
impl EdenapiSender for FilterEdenapiSender {
    async fn upload_contents(&self, contents: Vec<(AnyFileContentId, Bytes)>) -> Result<()> {
        if self
            .allowed
            .get(&MethodFilter::UploadContents)
            .map_or(false, |v| *v)
        {
            return self.inner.upload_contents(contents).await;
        } else {
            Ok(())
        }
    }

    async fn upload_trees(&self, trees: Vec<HgManifestId>) -> Result<()> {
        if self
            .allowed
            .get(&MethodFilter::UploadTrees)
            .map_or(false, |v| *v)
        {
            return self.inner.upload_trees(trees).await;
        } else {
            Ok(())
        }
    }

    async fn upload_filenodes(&self, fn_ids: Vec<HgFileNodeId>) -> Result<()> {
        if self
            .allowed
            .get(&MethodFilter::UploadFilenodes)
            .map_or(false, |v| *v)
        {
            return self.inner.upload_filenodes(fn_ids).await;
        } else {
            Ok(())
        }
    }

    async fn set_bookmark(&self, bookmark: String, moves: Vec<BookmarkMove>) -> Result<()> {
        if self
            .allowed
            .get(&MethodFilter::SetBookmark)
            .map_or(false, |v| *v)
        {
            return self.inner.set_bookmark(bookmark, moves).await;
        } else {
            Ok(())
        }
    }

    async fn upload_identical_changeset(
        &self,
        css: Vec<(HgBlobChangeset, BonsaiChangeset)>,
    ) -> Result<()> {
        if self
            .allowed
            .get(&MethodFilter::UploadIdenticalChangeset)
            .map_or(false, |v| *v)
        {
            return self.inner.upload_identical_changeset(css).await;
        } else {
            Ok(())
        }
    }

    async fn filter_existing_commits(
        &self,
        ids: Vec<(HgChangesetId, ChangesetId)>,
    ) -> Result<Vec<ChangesetId>> {
        if self
            .allowed
            .get(&MethodFilter::FilterExistingCommits)
            .map_or(false, |v| *v)
        {
            return self.inner.filter_existing_commits(ids).await;
        } else {
            Ok(ids.into_iter().map(|(_, cs_id)| cs_id).collect())
        }
    }

    async fn read_bookmark(&self, bookmark: String) -> Result<Option<HgChangesetId>> {
        if self
            .allowed
            .get(&MethodFilter::ReadBookmark)
            .map_or(false, |v| *v)
        {
            return self.inner.read_bookmark(bookmark).await;
        } else {
            Ok(None)
        }
    }

    async fn filter_existing_contents(&self, ids: Vec<ContentId>) -> Result<Vec<ContentId>> {
        if self
            .allowed
            .get(&MethodFilter::FilterExistingContents)
            .map_or(false, |v| *v)
        {
            return self.inner.filter_existing_contents(ids).await;
        } else {
            Ok(ids)
        }
    }
}
