/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use backsyncer::advance_bookmark_counter;
use bookmarks::BookmarkUpdateLogId;
use context::CoreContext;
use futures::channel::oneshot;
use itertools::Itertools;
use mercurial_types::blobs::HgBlobChangeset;
use metaconfig_types::ModernSyncChannelConfig;
use mononoke_types::BonsaiChangeset;
use mutable_counters::ArcMutableCounters;
use mutable_counters::MutableCounters;
use stats::define_stats;
use stats::prelude::*;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::sender::edenapi::BookmarkMove;
use crate::sender::edenapi::EdenapiSender;
use crate::sender::manager::BookmarkInfo;
use crate::sender::manager::ChangesetMessage;
use crate::sender::manager::Manager;
use crate::sender::manager::ModernSyncCounterNames;
use crate::stat;

define_stats! {
    prefix = "mononoke.modern_sync.manager.changeset";

    synced_commits:  dynamic_timeseries("{}.commits_synced", (repo: String); Sum),
    sync_lag_seconds:  dynamic_timeseries("{}.sync_lag_seconds", (repo: String); Average),
    trees_files_wait_time_s:  dynamic_timeseries("{}.trees_files_wait_time_s", (repo: String); Average),
    changeset_upload_time_s:  dynamic_timeseries("{}.changeset_upload_time_s", (repo: String); Average),

    changesets_queue_capacity: dynamic_singleton_counter("{}.changesets.queue_capacity", (repo: String)),
    changesets_queue_len: dynamic_quantile_stat("{}.changesets.queue_len", (repo: String); Average; P 50, P 75, P 95, P 99; Duration::from_secs(60), Duration::from_secs(600), Duration::from_secs(3600)),
    changesets_queue_max_capacity: dynamic_singleton_counter("{}.changesets.queue_max_capacity", (repo: String)),
}

async fn advance_completed_counter(
    ctx: &CoreContext,
    counters: &ArcMutableCounters,
    counter_name: &str,
    target: i64,
) -> Result<BookmarkUpdateLogId> {
    let target = BookmarkUpdateLogId(target.try_into()?);
    loop {
        let Some(counter) = counters.get_counter(ctx, counter_name).await? else {
            if counters
                .set_counter_if_absent(ctx, counter_name, target.try_into()?)
                .await?
            {
                return Ok(target);
            }
            continue;
        };

        let mut counter = counter.try_into()?;
        advance_bookmark_counter(ctx, counters, counter_name, &mut counter, target).await?;
        return Ok(counter);
    }
}

pub(crate) struct ChangesetManager {
    config: ModernSyncChannelConfig,
    changeset_recv: mpsc::Receiver<ChangesetMessage>,
    mc: Arc<dyn MutableCounters + Send + Sync>,
}

impl ChangesetManager {
    pub(crate) fn new(
        config: ModernSyncChannelConfig,
        changeset_recv: mpsc::Receiver<ChangesetMessage>,
        mc: Arc<dyn MutableCounters + Send + Sync>,
    ) -> Self {
        Self {
            config,
            changeset_recv,
            mc,
        }
    }

    async fn flush_batch(
        reponame: String,
        ctx: &CoreContext,
        changeset_es: &Arc<dyn EdenapiSender + Send + Sync>,
        mc: Arc<dyn MutableCounters + Send + Sync>,
        current_batch: &mut Vec<(HgBlobChangeset, BonsaiChangeset)>,
        pending_log: &mut VecDeque<Option<i64>>,
        latest_checkpoints: &mut HashMap<ModernSyncCounterNames, (u64, i64)>,
        latest_entry_ids: &mut HashMap<ModernSyncCounterNames, i64>,
        latest_bookmark_chain: &mut Vec<BookmarkInfo>,
        pending_notification: &mut Option<oneshot::Sender<Result<()>>>,
    ) -> Result<(), anyhow::Error> {
        if !current_batch.is_empty() {
            let start = std::time::Instant::now();
            let batch_size = current_batch.len();
            if let Err(e) = changeset_es
                .upload_identical_changeset(std::mem::take(current_batch))
                .await
            {
                tracing::error!("Failed to upload changesets {:?} {:?}", current_batch, e);
                return Err(e);
            } else {
                let elapsed = start.elapsed().as_secs() / batch_size as u64;
                STATS::changeset_upload_time_s.add_value(elapsed as i64, (reponame.clone(),));
                STATS::synced_commits.add_value(batch_size as i64, (reponame.clone(),));
            }
        }

        while let Some(Some(lag)) = pending_log.pop_front() {
            STATS::sync_lag_seconds.add_value(lag, (reponame.clone(),));
        }

        for (counter_names, (position, id)) in latest_checkpoints.drain() {
            tracing::info!("Setting checkpoint from entry {} to {}", id, position);

            let res_entry = mc
                .set_counter(ctx, &counter_names.current_entry, id, None)
                .await?;

            let res_checkpoint = mc
                .set_counter(
                    ctx,
                    &counter_names.batch_checkpoint,
                    position.try_into().unwrap(),
                    None,
                )
                .await?;

            if !(res_checkpoint && res_entry) {
                tracing::warn!(
                    "Failed to checkpoint entry {} at position {:?}",
                    id,
                    position
                );
            }
        }

        if !latest_bookmark_chain.is_empty() {
            let chain = std::mem::take(latest_bookmark_chain);
            // A flush can hold moves for more than one bookmark, so group the
            // moves by bookmark name and replay each chain under its own name.
            // Preserve per-bookmark order for the compare-and-swap chain.
            let by_name = chain
                .into_iter()
                .into_group_map_by(|info| info.name.clone());
            for (name, infos) in by_name {
                // A deletion clears the bookmark. It must never reach the
                // shadow replica, so fail hard and loudly instead of removing
                // the replica's bookmark.
                let moves = infos
                    .iter()
                    .map(|info| {
                        anyhow::Ok(BookmarkMove {
                            log_id: info.log_id,
                            from: info.from_cs_id,
                            to: info.to_cs_id.ok_or_else(|| {
                                anyhow!(
                                    "refusing to mirror deletion of bookmark {} (log id {}): \
                                     a bookmark deletion must not propagate to the shadow replica",
                                    name,
                                    info.log_id
                                )
                            })?,
                            reason: info.reason,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                tracing::info!(
                    "Setting bookmark {} from {:?} to {:?} over {} move(s)",
                    name,
                    moves.first().and_then(|mv| mv.from),
                    moves.last().map(|mv| mv.to),
                    moves.len()
                );
                changeset_es.set_bookmark(name, moves).await?;
            }
        }

        for (counter_names, id) in latest_entry_ids.drain() {
            tracing::info!("Marking entry {} as done", id);
            let completed =
                advance_completed_counter(ctx, &mc, &counter_names.completed_entry, id).await?;
            if completed.0 as i64 > id {
                tracing::info!(
                    "Completed cursor {} is already at {}",
                    counter_names.completed_entry,
                    completed,
                );
            }
        }

        if let Some(sender) = pending_notification.take() {
            let _ = sender.send(Ok(()));
        }

        Ok(())
    }
}

impl Manager for ChangesetManager {
    async fn run(
        &mut self,
        ctx: CoreContext,
        reponame: String,
        changeset_es: Arc<dyn EdenapiSender + Send + Sync>,
        cancellation_requested: Arc<AtomicBool>,
    ) -> Result<()> {
        let changeset_recv = &mut self.changeset_recv;
        let mc = &self.mc;

        let mut encountered_error: Option<anyhow::Error> = None;

        let mut pending_log = VecDeque::new();

        let mut latest_in_entry_checkpoints = HashMap::new();
        let mut latest_entry_ids = HashMap::new();
        let mut latest_bookmark_chain: Vec<BookmarkInfo> = Vec::new();
        let mut pending_notification = None;

        let mut current_batch = Vec::new();
        let mut flush_timer = interval(Duration::from_millis(self.config.flush_interval_ms as u64));

        while !cancellation_requested.load(Ordering::Relaxed) {
            tokio::select! {

                msg = changeset_recv.recv() => {

                    tracing::debug!(
                        "Changeset channel capacity: {} max capacity: {} in queue: {}",
                        changeset_recv.capacity(),
                        self.config.channel_size,
                        changeset_recv.len()
                    );
                    STATS::changesets_queue_capacity.set_value(ctx.fb, changeset_recv.capacity() as i64, (reponame.clone(),));
                    STATS::changesets_queue_len.add_value(changeset_recv.len() as i64, (reponame.clone(),));
                    STATS::changesets_queue_max_capacity.set_value(ctx.fb, changeset_recv.max_capacity() as i64, (reponame.clone(),));
                    match msg {
                        Some(ChangesetMessage::WaitForFilesAndTrees(
                            files_receiver,
                            trees_receiver,
                        )) => {
                            // Read outcome from files and trees upload
                            let start = std::time::Instant::now();
                            match tokio::try_join!(files_receiver, trees_receiver) {
                                Ok((res_files, res_trees)) => {
                                    if res_files.is_err() || res_trees.is_err() {
                                        tracing::error!(
                                            "Error processing files/trees: {:?} {:?}",
                                            res_files,
                                            res_trees
                                        );
                                        encountered_error.get_or_insert(anyhow::anyhow!(
                                            "Files/trees error received. Winding down changesets sender.",
                                        ));
                                    }
                                    let elapsed = start.elapsed().as_secs();
                                    STATS::trees_files_wait_time_s
                                        .add_value(elapsed as i64, (reponame.clone(),));
                                }
                                Err(e) => {
                                    encountered_error.get_or_insert(anyhow::anyhow!(
                                        "Error waiting for files/trees error received {e:#}"
                                    ));
                                }
                            }
                        }

                        Some(ChangesetMessage::Changeset((hg_cs, bcs)))
                            if encountered_error.is_none() =>
                        {
                            current_batch.push((hg_cs, bcs));
                        }

                        Some(ChangesetMessage::CheckpointInEntry {
                            position,
                            entry_id,
                            counter_names,
                        })
                            if encountered_error.is_none() =>
                        {
                            latest_in_entry_checkpoints
                                .insert(counter_names, (position, entry_id));
                        }

                        Some(ChangesetMessage::FinishEntry(bookmark))
                            if encountered_error.is_none() =>
                        {
                            // Accumulate the contiguous chain of moves. The
                            // flush replays every move to the replica so it
                            // reuses each source log id.
                            latest_entry_ids
                                .insert(bookmark.counter_names.clone(), bookmark.log_id);
                            latest_bookmark_chain.push(bookmark);
                        }

                        Some(ChangesetMessage::NotifyCompletion(sender))
                            if encountered_error.is_none() =>
                        {
                            pending_notification = Some(sender);
                        }

                        Some(ChangesetMessage::NotifyCompletion(sender)) => {
                            let e = encountered_error.unwrap();
                            let _ = sender.send(Err(anyhow::anyhow!(
                                "Error processing changesets: {e:?}"
                            )));
                            return Err(e);
                        }

                        Some(ChangesetMessage::Log((_, lag)))
                            if encountered_error.is_none() =>
                        {
                            pending_log.push_back(lag);
                        }
                        None => break,

                        // Ignore any other action if there's an error
                        _ => {}
                    }

                    if current_batch.len() >= self.config.batch_size as usize {
                        let now = std::time::Instant::now();
                        let changeset_ids = current_batch.iter().map(|c| c.1.get_changeset_id()).collect::<Vec<_>>();
                        stat::log_upload_changeset_start(&ctx, changeset_ids.clone());
                        match ChangesetManager::flush_batch(reponame.clone(), &ctx, &changeset_es, mc.clone(), &mut current_batch, &mut pending_log, &mut latest_in_entry_checkpoints, &mut latest_entry_ids, &mut latest_bookmark_chain, &mut pending_notification)
                        .await
                        {
                            Ok(()) => {
                                stat::log_upload_changeset_done(&ctx, changeset_ids, now.elapsed());
                            }
                            Err(e) => {
                                stat::log_upload_changeset_error(&ctx, changeset_ids, &e, now.elapsed());
                                return Err(anyhow::anyhow!(
                                    "Error processing changesets: {e:?}"
                                ));
                            }
                        }
                    }
                }
                _ = flush_timer.tick() =>
                {
                    let now = std::time::Instant::now();
                    let changeset_ids = current_batch.iter().map(|c| c.1.get_changeset_id()).collect::<Vec<_>>();
                    stat::log_upload_changeset_start(&ctx, changeset_ids.clone());
                    match ChangesetManager::flush_batch(reponame.clone(), &ctx, &changeset_es, mc.clone(), &mut current_batch, &mut pending_log, &mut latest_in_entry_checkpoints, &mut latest_entry_ids, &mut latest_bookmark_chain, &mut pending_notification)
                    .await
                    {
                        Ok(()) => {
                                stat::log_upload_changeset_done(&ctx, changeset_ids, now.elapsed());
                        }
                        Err(e) => {
                            stat::log_upload_changeset_error(&ctx, changeset_ids, &e, now.elapsed());
                            return Err(anyhow::anyhow!("Error processing changesets: {e:?}"));
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;

    use async_trait::async_trait;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    #[derive(Default)]
    struct TestMutableCounters {
        counters: RwLock<HashMap<String, i64>>,
    }

    #[async_trait]
    impl MutableCounters for TestMutableCounters {
        async fn get_counter(&self, _ctx: &CoreContext, name: &str) -> Result<Option<i64>> {
            Ok(self
                .counters
                .read()
                .expect("test counter lock poisoned")
                .get(name)
                .copied())
        }

        async fn get_maybe_stale_counter(
            &self,
            ctx: &CoreContext,
            name: &str,
        ) -> Result<Option<i64>> {
            self.get_counter(ctx, name).await
        }

        async fn set_counter(
            &self,
            _ctx: &CoreContext,
            name: &str,
            value: i64,
            prev_value: Option<i64>,
        ) -> Result<bool> {
            let mut counters = self.counters.write().expect("test counter lock poisoned");
            if let Some(prev_value) = prev_value
                && counters.get(name).copied() != Some(prev_value)
            {
                return Ok(false);
            }
            counters.insert(name.to_string(), value);
            Ok(true)
        }

        async fn set_counters(&self, _ctx: &CoreContext, counters: &[(&str, i64)]) -> Result<()> {
            self.counters
                .write()
                .expect("test counter lock poisoned")
                .extend(
                    counters
                        .iter()
                        .map(|(name, value)| ((*name).to_owned(), *value)),
                );
            Ok(())
        }

        async fn set_counter_if_absent(
            &self,
            _ctx: &CoreContext,
            name: &str,
            value: i64,
        ) -> Result<bool> {
            let mut counters = self.counters.write().expect("test counter lock poisoned");
            if counters.contains_key(name) {
                return Ok(false);
            }
            counters.insert(name.to_string(), value);
            Ok(true)
        }

        async fn get_all_counters(&self, _ctx: &CoreContext) -> Result<Vec<(String, i64)>> {
            Ok(self
                .counters
                .read()
                .expect("test counter lock poisoned")
                .iter()
                .map(|(name, value)| (name.clone(), *value))
                .collect())
        }
    }

    #[mononoke::fbinit_test]
    async fn completed_counter_never_regresses(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let counters: ArcMutableCounters = Arc::new(TestMutableCounters::default());

        assert_eq!(
            advance_completed_counter(&ctx, &counters, "completed", 100).await?,
            BookmarkUpdateLogId(100),
        );
        assert_eq!(
            advance_completed_counter(&ctx, &counters, "completed", 120).await?,
            BookmarkUpdateLogId(120),
        );

        counters
            .set_counter(&ctx, "completed", 130, Some(120))
            .await?;
        assert_eq!(
            advance_completed_counter(&ctx, &counters, "completed", 110).await?,
            BookmarkUpdateLogId(130),
        );
        assert_eq!(counters.get_counter(&ctx, "completed").await?, Some(130));

        Ok(())
    }
}
