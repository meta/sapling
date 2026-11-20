/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod builder;
mod connections;
mod local_cache;
mod reader;
mod remote_cache;
mod shards;
mod sql_timeout_knobs;
mod writer;

#[cfg(test)]
mod test;

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
pub use builder::NewFilenodesBuilder;
use context::CoreContext;
use filenodes::FilenodeInfo;
use filenodes::FilenodeRange;
use filenodes::FilenodeResult;
use filenodes::Filenodes;
use filenodes::PreparedFilenode;
use mercurial_types::HgFileNodeId;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
pub use path_hash::PathHash;
use reader::FilenodesReader;
pub use sql_timeout_knobs::disable_sql_timeouts;
use writer::FilenodesWriter;

#[derive(Clone)]
pub struct NewFilenodes {
    reader: Arc<FilenodesReader>,
    writer: Arc<FilenodesWriter>,
    repo_id: RepositoryId,
}

#[async_trait]
impl Filenodes for NewFilenodes {
    async fn add_filenodes(
        &self,
        ctx: &CoreContext,
        info: Vec<PreparedFilenode>,
    ) -> Result<FilenodeResult<()>> {
        let ret = self
            .writer
            .insert_filenodes(ctx, self.repo_id, info, false /* replace */)
            .await
            .context("Internal error: failure while inserting filenodes")?;
        Ok(ret)
    }

    async fn add_or_replace_filenodes(
        &self,
        ctx: &CoreContext,
        info: Vec<PreparedFilenode>,
    ) -> Result<FilenodeResult<()>> {
        let ret = self
            .writer
            .insert_filenodes(ctx, self.repo_id, info, true /* replace */)
            .await
            .context("Internal error: failure while inserting filenodes")?;
        Ok(ret)
    }

    async fn get_filenode(
        &self,
        ctx: &CoreContext,
        path: &RepoPath,
        filenode_id: HgFileNodeId,
    ) -> Result<FilenodeResult<Option<FilenodeInfo>>> {
        let ret = self
            .reader
            .clone()
            .get_filenode(ctx, self.repo_id, path, filenode_id)
            .await
            .with_context(|| {
                anyhow!("Internal error: failure while fetching file node {filenode_id} {path}")
            })?;
        Ok(ret)
    }

    async fn get_all_filenodes_maybe_stale(
        &self,
        ctx: &CoreContext,
        path: &RepoPath,
        limit: Option<u64>,
    ) -> Result<FilenodeResult<FilenodeRange>> {
        let ret = self
            .reader
            .clone()
            .get_all_filenodes_for_path(ctx, self.repo_id, path, limit)
            .await
            .with_context(|| {
                anyhow!("Internal error: failure while fetching file nodes for {path}")
            })?;
        Ok(ret)
    }
}
