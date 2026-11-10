/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use async_trait::async_trait;
use bookmarks::BookmarkUpdateReason;
use edenapi_types::AnyFileContentId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::blobs::HgBlobChangeset;
use minibytes::Bytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use thiserror::Error;

mod config;
mod default;
mod filter;
mod noop;
mod retry;
mod util;

pub use config::EdenapiConfig;
pub(crate) use default::DefaultEdenapiSenderBuilder;
pub(crate) use filter::FilterEdenapiSender;
pub(crate) use filter::MethodFilter;
pub(crate) use noop::NoopEdenapiSender;
pub(crate) use retry::RetryEdenapiSender;

/// One bookmark move in a contiguous chain, taken from a source
/// bookmarks_update_log entry. The mirror path replays the whole chain to a
/// `*_shadow` replica and reuses each move's `log_id` and `reason`, so the
/// replica's log matches the source row for row.
///
/// The changesets stay in the bonsai form the source log holds them in.
/// modern_sync uploads each changeset to the replica under the source's own
/// bonsai id, so the replica resolves the same ids and neither end translates.
///
/// `to` is required. A source move that clears the bookmark is a deletion,
/// which must never reach the shadow replica, so the mirror path rejects it
/// where it reads the log entry rather than carrying it this far.
#[derive(Clone)]
pub(crate) struct BookmarkMove {
    pub log_id: i64,
    pub from: Option<ChangesetId>,
    pub to: ChangesetId,
    pub reason: BookmarkUpdateReason,
}

/// Context on the error `set_bookmark` returns when the replica answered and
/// refused the moves. Such a refusal is final: the sync must stop rather than
/// send the same moves again.
#[derive(Debug, Error)]
#[error("server rejected mirrored bookmark moves")]
pub(crate) struct BookmarkMovesRejected;

#[async_trait]
pub(crate) trait EdenapiSender {
    async fn upload_contents(&self, contents: Vec<(AnyFileContentId, Bytes)>) -> Result<()>;

    async fn upload_trees(&self, trees: Vec<HgManifestId>) -> Result<()>;

    async fn upload_filenodes(&self, fn_ids: Vec<HgFileNodeId>) -> Result<()>;

    // Move a bookmark over a contiguous chain of source moves. `moves` is
    // non-empty and ordered by strictly increasing `log_id`.
    async fn set_bookmark(&self, bookmark: String, moves: Vec<BookmarkMove>) -> Result<()>;

    async fn upload_identical_changeset(
        &self,
        css: Vec<(HgBlobChangeset, BonsaiChangeset)>,
    ) -> Result<()>;

    async fn filter_existing_commits(
        &self,
        ids: Vec<(HgChangesetId, ChangesetId)>,
    ) -> Result<Vec<ChangesetId>>;

    async fn read_bookmark(&self, bookmark: String) -> Result<Option<HgChangesetId>>;
}
