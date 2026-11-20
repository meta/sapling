/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::panic::AssertUnwindSafe;
use std::time::Duration;

use bookmarks::BookmarksRef;
use lock_ext::RwLockExt;
use tokio::time::Instant;

use super::*;

const CONCURRENT_WARMS: usize = 20;
#[cfg(not(fbcode_build))]
const POLL_INTERVAL: Duration = Duration::from_secs(5);

fn poll_interval() -> Duration {
    #[cfg(fbcode_build)]
    {
        Duration::from_millis(justknobs::get_as::<u64>(
            "scm/mononoke:warm_bookmark_cache_poll_interval_ms",
            None,
        ))
    }
    #[cfg(not(fbcode_build))]
    {
        POLL_INTERVAL
    }
}

// This tracks the oldest observed unresolved mismatch, not the bookmark's
// update-log timestamp. Reports occur only at poll boundaries; a blocked warming
// batch does not have a staleness heartbeat.
fn report_observed_lag(
    ctx: &CoreContext,
    repo_name: &str,
    unresolved: &HashMap<BookmarkKey, Instant>,
) {
    let seconds = unresolved
        .values()
        .map(|observed| observed.elapsed().as_secs())
        .max()
        .unwrap_or(0);
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    STATS::max_staleness_secs.set_value(ctx.fb, seconds, (repo_name.to_owned(),));
    STATS::global_max_staleness_secs.add_value(seconds);
    #[cfg(fbcode_build)]
    WBC_INSTRUMENT.observe(MononokeWarmBookmarkCacheStats {
        repo: Some(repo_name.to_owned()),
        max_staleness_secs: Some(seconds as f64),
        ..Default::default()
    });
}

pub(super) async fn read_bookmarks(
    ctx: &CoreContext,
    bookmarks: &dyn Bookmarks,
) -> Result<HashMap<BookmarkKey, (ChangesetId, BookmarkKind)>, Error> {
    bookmarks
        .list(
            ctx.clone(),
            Freshness::MostRecent,
            &BookmarkPrefix::empty(),
            BookmarkCategory::ALL,
            BookmarkKind::ALL_PUBLISHING,
            &BookmarkPagination::FromStart,
            u64::MAX,
        )
        .map_ok(|(bookmark, changeset_id)| {
            let kind = *bookmark.kind();
            (bookmark.into_key(), (changeset_id, kind))
        })
        .try_collect()
        .await
        .context("Reading current bookmarks for polling")
}

impl BookmarksCoordinator {
    pub(super) async fn poll_bookmarks(
        &self,
        ctx: &CoreContext,
        unresolved: &mut HashMap<BookmarkKey, Instant>,
    ) -> Result<(), Error> {
        let latest = match read_bookmarks(ctx, self.repo.bookmarks()).await {
            Ok(latest) => latest,
            Err(error) => {
                report_observed_lag(ctx, self.repo.repo_identity().name(), unresolved);
                return Err(error);
            }
        };
        unresolved.retain(|key, _| latest.contains_key(key));
        let observed_at = Instant::now();
        let updates = {
            let mut cached = self
                .bookmarks
                .write()
                .map_err(|e| anyhow!("Locking the bookmark cache: {e}"))?;
            cached.retain(|key, _| latest.contains_key(key));
            latest
                .into_iter()
                .map(|(key, (changeset_id, kind))| {
                    if let Some(state) = cached.get(&key)
                        && state.kind == kind
                        && state.get(WarmerRequirement::AllKinds)? == Some(changeset_id)
                    {
                        unresolved.remove(&key);
                        Ok(None)
                    } else {
                        unresolved.entry(key.clone()).or_insert(observed_at);
                        Ok(Some((key, changeset_id, kind)))
                    }
                })
                .collect::<Result<Vec<_>, Error>>()?
        };
        report_observed_lag(ctx, self.repo.repo_identity().name(), unresolved);
        let warmed = stream::iter(updates.into_iter().flatten())
            .map(|(key, changeset_id, kind)| async move {
                let ctx = ctx.with_mutated_scuba(|mut scuba| {
                    scuba.add("bookmark", key.to_string());
                    scuba.add("top_changeset", changeset_id.to_hex().to_string());
                    scuba
                });
                let result = AssertUnwindSafe(warm_all(&ctx, changeset_id, &self.warmers))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("bookmark warmer panicked")));
                match result {
                    Ok(()) => {
                        let mut state = BookmarkState::from_warmers(kind, &self.warmers);
                        state.set_all_tracked(changeset_id);
                        self.bookmarks
                            .with_write(|cached| cached.insert(key.clone(), state));
                        Some(key)
                    }
                    Err(error) => {
                        tracing::warn!(
                            repo = %self.repo.repo_identity().name(),
                            bookmark = %key,
                            %changeset_id,
                            ?error,
                            "bookmark warming failed; retrying on the next poll"
                        );
                        None
                    }
                }
            })
            .buffer_unordered(CONCURRENT_WARMS)
            .collect::<Vec<_>>()
            .await;
        for key in warmed.into_iter().flatten() {
            unresolved.remove(&key);
        }
        report_observed_lag(ctx, self.repo.repo_identity().name(), unresolved);
        Ok(())
    }

    pub(super) fn spawn_polling(
        self,
        ctx: CoreContext,
        terminate: oneshot::Receiver<()>,
        sync: SyncSignals,
    ) {
        let span = tracing::info_span!("wbc", repo = %self.repo.repo_identity().name());
        let future = async move {
            let mut unresolved = HashMap::new();
            let polling = async {
                let mut syncing = false;
                loop {
                    let ctx = ctx.with_mutated_scuba(|scuba| scuba.with_seq("seq"));
                    if let Err(error) = self.poll_bookmarks(&ctx, &mut unresolved).await {
                        tracing::warn!(?error, "bookmark poll failed");
                    }
                    if syncing {
                        syncing = false;
                        sync.complete.notify_waiters();
                    }
                    let delay = poll_interval();
                    tokio::select! {
                        _ = sync.start.notified() => syncing = true,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
            .boxed();
            let _ = select(polling, terminate).await;
            unresolved.clear();
            report_observed_lag(&ctx, self.repo.repo_identity().name(), &unresolved);
        }
        .instrument(span);
        std::mem::drop(mononoke::spawn_task(future));
    }
}

#[cfg(test)]
#[path = "polling_tests.rs"]
mod tests;
