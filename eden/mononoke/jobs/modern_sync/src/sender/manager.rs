/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateReason;
use context::CoreContext;
use futures::channel::oneshot;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::blobs::HgBlobChangeset;
use metaconfig_types::ModernSyncConfig;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mutable_counters::MutableCounters;
use mutable_counters::validate_counter_name;
use repo_blobstore::RepoBlobstore;
use tokio::sync::mpsc;

use crate::sender::edenapi::EdenapiSender;
use crate::sender::manager::changeset::ChangesetManager;
use crate::sender::manager::content::ContentManager;
use crate::sender::manager::filenode::FilenodeManager;
use crate::sender::manager::tree::TreeManager;

mod changeset;
mod content;
mod filenode;
mod pipeline;
mod tree;

pub(crate) const MODERN_SYNC_COUNTER_NAME: &str = "modern_sync";
pub(crate) const MODERN_SYNC_BATCH_CHECKPOINT_NAME: &str = "modern_sync_batch_checkpoint";
pub(crate) const MODERN_SYNC_CURRENT_ENTRY_ID: &str = "modern_sync_batch_id";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModernSyncCounterNames {
    pub completed_entry: String,
    pub batch_checkpoint: String,
    pub current_entry: String,
}

impl ModernSyncCounterNames {
    pub fn legacy() -> Self {
        Self {
            completed_entry: MODERN_SYNC_COUNTER_NAME.to_owned(),
            batch_checkpoint: MODERN_SYNC_BATCH_CHECKPOINT_NAME.to_owned(),
            current_entry: MODERN_SYNC_CURRENT_ENTRY_ID.to_owned(),
        }
    }

    pub fn for_bookmark(bookmark: &BookmarkKey) -> Result<Self> {
        let suffix = format!("{}_{}", bookmark.category(), bookmark.as_str());
        let counter_names = Self {
            completed_entry: format!("modern_sync_by_bookmark_v1_{suffix}"),
            batch_checkpoint: format!("modern_sync_batch_checkpoint_by_bookmark_v1_{suffix}"),
            current_entry: format!("modern_sync_batch_id_by_bookmark_v1_{suffix}"),
        };
        validate_counter_name(&counter_names.completed_entry)?;
        validate_counter_name(&counter_names.batch_checkpoint)?;
        validate_counter_name(&counter_names.current_entry)?;
        Ok(counter_names)
    }
}

/// Runs each manager as its own task, with the arguments every manager
/// shares.
struct ManagerSpawner {
    ctx: CoreContext,
    reponame: String,
    external_sender: Arc<dyn EdenapiSender + Send + Sync>,
    cancellation_requested: Arc<AtomicBool>,
}

impl ManagerSpawner {
    fn spawn<M>(&self, mut manager: M)
    where
        M: Manager + Send + 'static,
    {
        let ctx = self.ctx.clone();
        let reponame = self.reponame.clone();
        let external_sender = self.external_sender.clone();
        let cancellation_requested = self.cancellation_requested.clone();
        mononoke::spawn_task(async move {
            let _ = manager
                .run(ctx, reponame, external_sender, cancellation_requested)
                .await;
        });
    }
}

#[derive(Clone)]
pub struct SendManager {
    content_sender: mpsc::Sender<ContentMessage>,
    files_sender: mpsc::Sender<FileMessage>,
    trees_sender: mpsc::Sender<TreeMessage>,
    changeset_sender: mpsc::Sender<ChangesetMessage>,
}

pub enum ContentMessage {
    // Send the content to remote end
    Content(ContentId, u64),
    // Finished sending content of a changeset. Go ahead with files and trees
    ContentDone(oneshot::Sender<Result<()>>, oneshot::Sender<Result<()>>),
}

#[derive(Default)]
pub struct Messages {
    pub content_messages: Vec<ContentMessage>,
    pub trees_messages: Vec<TreeMessage>,
    pub files_messages: Vec<FileMessage>,
    pub changeset_messages: Vec<ChangesetMessage>,
}

pub enum TreeMessage {
    // Wait for contents to be sent before sending trees
    WaitForContents(oneshot::Receiver<Result<()>>),
    // Send the tree to remote end
    Tree(HgManifestId),
    // Finished sending trees. Go ahead with changesets
    TreesDone(oneshot::Sender<Result<()>>),
}

pub enum FileMessage {
    // Wait for contents to be sent before sending files
    WaitForContents(oneshot::Receiver<Result<()>>),
    // Send the file node to remote end
    FileNode(HgFileNodeId),
    // Finished sending files. Go ahead with changesets
    FilesDone(oneshot::Sender<Result<()>>),
}

pub enum ChangesetMessage {
    // Wait for files and trees to be sent before sending changesets
    WaitForFilesAndTrees(oneshot::Receiver<Result<()>>, oneshot::Receiver<Result<()>>),
    // Send the changeset to remote end
    Changeset((HgBlobChangeset, BonsaiChangeset)),
    // Checkpoint position (first argument) within the BUL entry (second argument)
    CheckpointInEntry {
        position: u64,
        entry_id: i64,
        counter_names: ModernSyncCounterNames,
    },
    // Perform bookmark movement and mark BUL entry as completed once the changeset is synced
    FinishEntry(BookmarkInfo),
    // Notify changeset sending is done
    NotifyCompletion(oneshot::Sender<Result<()>>),
    // Log changeset completion
    Log((String, Option<i64>)),
}

pub struct BookmarkInfo {
    pub name: String,
    pub from_cs_id: Option<ChangesetId>,
    pub to_cs_id: Option<ChangesetId>,
    // Source bookmarks_update_log id and reason for this move. The mirror path
    // reuses both so the replica's log row matches the source row for row.
    pub log_id: i64,
    pub reason: BookmarkUpdateReason,
    pub counter_names: ModernSyncCounterNames,
}

impl SendManager {
    pub fn new(
        ctx: CoreContext,
        config: &ModernSyncConfig,
        repo_blobstore: RepoBlobstore,
        external_sender: Arc<dyn EdenapiSender + Send + Sync>,
        reponame: String,
        exit_file: PathBuf,
        mc: Arc<dyn MutableCounters + Send + Sync>,
        cancellation_requested: Arc<AtomicBool>,
    ) -> Self {
        let managers = ManagerSpawner {
            ctx,
            reponame,
            external_sender,
            cancellation_requested: cancellation_requested.clone(),
        };

        // Create channel for receiving content
        let (content_sender, content_recv) = mpsc::channel(
            config
                .content_channel_config
                .channel_size
                .try_into()
                .expect("channel size is too large"),
        );
        managers.spawn(ContentManager::new(
            config.max_blob_bytes as u64,
            config.content_channel_config.clone(),
            content_recv,
            repo_blobstore,
        ));

        // Create channel for receiving files
        let (files_sender, files_recv) = mpsc::channel(
            config
                .filenodes_channel_config
                .channel_size
                .try_into()
                .expect("channel size is too large"),
        );
        managers.spawn(FilenodeManager::new(
            config.filenodes_channel_config.clone(),
            files_recv,
        ));

        // Create channel for receiving trees
        let (trees_sender, trees_recv) = mpsc::channel(
            config
                .trees_channel_config
                .channel_size
                .try_into()
                .expect("channel size is too large"),
        );
        managers.spawn(TreeManager::new(
            config.trees_channel_config.clone(),
            trees_recv,
        ));

        // Create channel for receiving changesets
        let (changeset_sender, changeset_recv) = mpsc::channel(
            config
                .changesets_channel_config
                .channel_size
                .try_into()
                .expect("channel size is too large"),
        );
        managers.spawn(ChangesetManager::new(
            config.changesets_channel_config.clone(),
            changeset_recv,
            mc,
        ));

        mononoke::spawn_task(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                if fs::metadata(exit_file.clone()).is_ok() {
                    tracing::warn!("Exit file detected, stopping sync");
                    cancellation_requested.store(true, Ordering::Relaxed);
                    break;
                }
            }
        });

        Self {
            content_sender,
            files_sender,
            trees_sender,
            changeset_sender,
        }
    }

    pub async fn send_content(&self, content_msg: ContentMessage) -> Result<()> {
        self.content_sender
            .send(content_msg)
            .await
            .map_err(|err| err.into())
    }

    pub async fn send_file(&self, ft_msg: FileMessage) -> Result<()> {
        self.files_sender
            .send(ft_msg)
            .await
            .map_err(|err| err.into())
    }

    pub async fn send_tree(&self, ft_msg: TreeMessage) -> Result<()> {
        self.trees_sender
            .send(ft_msg)
            .await
            .map_err(|err| err.into())
    }

    pub async fn send_changeset(&self, cs_msg: ChangesetMessage) -> Result<()> {
        self.changeset_sender
            .send(cs_msg)
            .await
            .map_err(|err| err.into())
    }

    pub async fn send_contents(&self, content_msgs: Vec<ContentMessage>) -> Result<()> {
        for content_msg in content_msgs {
            self.send_content(content_msg).await?;
        }
        Ok(())
    }

    pub async fn send_files(&self, ft_msgs: Vec<FileMessage>) -> Result<()> {
        for ft_msg in ft_msgs {
            self.send_file(ft_msg).await?;
        }
        Ok(())
    }

    pub async fn send_trees(&self, ft_msgs: Vec<TreeMessage>) -> Result<()> {
        for ft_msg in ft_msgs {
            self.send_tree(ft_msg).await?;
        }
        Ok(())
    }

    pub async fn send_changesets(&self, cs_msgs: Vec<ChangesetMessage>) -> Result<()> {
        for cs_msg in cs_msgs {
            self.send_changeset(cs_msg).await?;
        }
        Ok(())
    }
}

trait Manager {
    /// Runs the manager's loop until it stops.
    fn run(
        &mut self,
        ctx: CoreContext,
        reponame: String,
        external_sender: Arc<dyn EdenapiSender + Send + Sync>,
        cancellation_requested: Arc<AtomicBool>,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// What a manager loop should do after handling one select event.
pub(crate) enum LoopAction {
    Continue,
    /// Stop the loop and exit the runner.
    Exit,
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use bookmarks::BookmarkCategory;
    use bookmarks::BookmarkName;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn bookmark_counter_names_are_readable_and_category_specific() -> Result<()> {
        let branch = BookmarkKey::new("feature/modern-sync")?;
        let counters = ModernSyncCounterNames::for_bookmark(&branch)?;
        assert_eq!(
            counters.completed_entry,
            "modern_sync_by_bookmark_v1_branch_feature/modern-sync"
        );
        assert_eq!(
            counters.batch_checkpoint,
            "modern_sync_batch_checkpoint_by_bookmark_v1_branch_feature/modern-sync"
        );
        assert_eq!(
            counters.current_entry,
            "modern_sync_batch_id_by_bookmark_v1_branch_feature/modern-sync"
        );

        let tag = BookmarkKey::with_name_and_category(
            BookmarkName::new("feature/modern-sync")?,
            BookmarkCategory::Tag,
        );
        assert_ne!(
            counters.completed_entry,
            ModernSyncCounterNames::for_bookmark(&tag)?.completed_entry
        );
        Ok(())
    }
}
