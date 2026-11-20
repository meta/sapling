/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Mutex;
#[cfg(not(fbcode_build))]
use std::task::Poll;

use bookmarks::BookmarkUpdateLogArc;
use bookmarks::BookmarksArc;
use cloned::cloned;
use fbinit::FacebookInit;
#[cfg(not(fbcode_build))]
use futures::future::poll_fn;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use repo_event_publisher::RepoEventPublisherArc;
use repo_identity::RepoIdentityArc;
use sql_ext::mononoke_queries;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;
use crate::tests::Repo;

mononoke_queries! {
    write DropUpdateLog() {
        none,
        "DROP TABLE bookmarks_update_log"
    }
    write DropBookmarks() {
        none,
        "DROP TABLE bookmarks"
    }
    write SetCurrentHash(repo_id: RepositoryId, name: String, hash: ChangesetId, kind: BookmarkKind) {
        none,
        "UPDATE bookmarks SET changeset_id = {hash}, hg_kind = {kind} WHERE repo_id = {repo_id} AND name = {name}"
    }
    write DeleteCurrent(repo_id: RepositoryId, name: String) {
        none,
        "DELETE FROM bookmarks WHERE repo_id = {repo_id} AND name = {name}"
    }
}

#[derive(Clone, Copy)]
enum WarmFailure {
    Error,
    Panic,
}

struct Gate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct Fixture {
    factory: TestRepoFactory,
    repo: Repo,
    ctx: CoreContext,
    coordinator: BookmarksCoordinator,
    calls: Arc<Mutex<HashMap<ChangesetId, usize>>>,
    failures: Arc<Mutex<HashMap<ChangesetId, WarmFailure>>>,
    gates: Arc<Mutex<HashMap<ChangesetId, Gate>>>,
}

impl Fixture {
    async fn new(fb: FacebookInit) -> Result<Self, Error> {
        Self::named(fb, "repo").await
    }

    async fn named(fb: FacebookInit, name: &str) -> Result<Self, Error> {
        let mut factory = TestRepoFactory::new(fb)?;
        factory.with_name(name);
        let repo: Repo = factory.build().await?;
        let ctx = CoreContext::test_mock(fb);
        let calls = Arc::new(Mutex::new(HashMap::new()));
        let failures = Arc::new(Mutex::new(HashMap::new()));
        let gates = Arc::new(Mutex::new(HashMap::<_, Gate>::new()));
        let warmer = Warmer {
            name: "OSS snapshot test".to_owned(),
            tags: vec![WarmerTag::Hg, WarmerTag::Git],
            is_warm: Box::new(|_, _| async { Ok(true) }.boxed()),
            warmer: Box::new({
                cloned!(calls, failures, gates);
                move |_, hash| {
                    cloned!(calls, failures, gates);
                    async move {
                        *calls.lock().expect("warmer calls").entry(hash).or_insert(0) += 1;
                        let gate = gates.lock().expect("warmer gates").remove(&hash);
                        if let Some(Gate { entered, release }) = gate {
                            let _ = entered.send(());
                            release.await.context("releasing the blocked warmer")?;
                        }
                        let failure = failures
                            .lock()
                            .expect("warmer failures")
                            .get(&hash)
                            .copied();
                        match failure {
                            Some(WarmFailure::Error) => Err(anyhow!("injected warmer failure")),
                            Some(WarmFailure::Panic) => panic!("injected warmer panic"),
                            None => Ok(()),
                        }
                    }
                    .boxed()
                }
            }),
        };
        let coordinator = BookmarksCoordinator::new(
            Arc::new(RwLock::new(HashMap::new())),
            repo.bookmarks_arc(),
            repo.bookmark_update_log_arc(),
            repo.repo_identity_arc(),
            repo.repo_event_publisher_arc(),
            Arc::new(vec![warmer]),
        );
        Ok(Self {
            factory,
            repo,
            ctx,
            coordinator,
            calls,
            failures,
            gates,
        })
    }

    async fn commit(&self, name: &str) -> Result<ChangesetId, Error> {
        CreateCommitContext::new_root(&self.ctx, &self.repo)
            .add_file(name, name)
            .commit()
            .await
    }

    async fn set(&self, name: &str, hash: ChangesetId, kind: BookmarkKind) -> Result<(), Error> {
        SetCurrentHash::query(
            &self.factory.metadata_db().write_connection,
            self.ctx.sql_query_telemetry(),
            &self.repo.repo_identity().id(),
            &name.to_owned(),
            &hash,
            &kind,
        )
        .await?;
        Ok(())
    }

    fn cached(&self, name: &str) -> Option<ChangesetId> {
        self.coordinator.bookmarks.with_read(|cached| {
            cached
                .get(&BookmarkKey::new(name).expect("test bookmark"))
                .and_then(|state| {
                    state
                        .get(WarmerRequirement::AllKinds)
                        .expect("all kinds tracked")
                })
        })
    }

    fn gate(&self, hash: ChangesetId) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, observed) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        self.gates.lock().expect("warmer gates").insert(
            hash,
            Gate {
                entered,
                release: blocked,
            },
        );
        (observed, release)
    }

    fn count(&self, hash: ChangesetId) -> usize {
        *self
            .calls
            .lock()
            .expect("warmer calls")
            .get(&hash)
            .unwrap_or(&0)
    }

    async fn drop_log(&self) -> Result<(), Error> {
        DropUpdateLog::query(
            &self.factory.metadata_db().write_connection,
            self.ctx.sql_query_telemetry(),
        )
        .await?;
        Ok(())
    }

    fn assert_observed_age(&self, unresolved: &HashMap<BookmarkKey, Instant>, expected: u64) {
        let seconds = unresolved
            .values()
            .map(|observed| observed.elapsed().as_secs())
            .max()
            .unwrap_or(0);
        assert_eq!(seconds, expected);
        #[cfg(fbcode_build)]
        assert_eq!(
            STATS::max_staleness_secs
                .get_value(self.ctx.fb, (self.repo.repo_identity().name().to_owned(),),),
            Some(expected as i64),
        );
    }
}

#[mononoke::fbinit_test]
async fn poll_current_hashes_and_deletions_without_an_update_log(
    fb: FacebookInit,
) -> Result<(), Error> {
    let f = Fixture::new(fb).await?;
    let first = f.commit("first").await?;
    let second = f.commit("second").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    f.drop_log().await?;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut HashMap::new())
        .await?;
    assert_eq!(f.cached("main"), Some(first));
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut HashMap::new())
        .await?;
    assert_eq!(f.count(first), 1, "unchanged hashes must not warm again");
    f.set("main", second, BookmarkKind::Publishing).await?;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut HashMap::new())
        .await?;
    assert_eq!(f.cached("main"), Some(second));
    assert_eq!(f.count(second), 1);
    DeleteCurrent::query(
        &f.factory.metadata_db().write_connection,
        f.ctx.sql_query_telemetry(),
        &f.repo.repo_identity().id(),
        &"main".to_owned(),
    )
    .await?;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut HashMap::new())
        .await?;
    assert_eq!(
        f.cached("main"),
        None,
        "deletion comes from the current snapshot"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn a_failed_warm_is_isolated_and_retried_on_the_next_poll(
    fb: FacebookInit,
) -> Result<(), Error> {
    for failure in [WarmFailure::Error, WarmFailure::Panic] {
        let f = Fixture::new(fb).await?;
        let first = f.commit("before").await?;
        let failed = f.commit("failed").await?;
        let good = f.commit("good").await?;
        bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
        bookmark(&f.ctx, &f.repo, "other").set_to(first).await?;
        f.drop_log().await?;
        f.coordinator
            .poll_bookmarks(&f.ctx, &mut HashMap::new())
            .await?;
        f.set("main", failed, BookmarkKind::Publishing).await?;
        f.set("other", good, BookmarkKind::Publishing).await?;
        f.failures
            .lock()
            .expect("warmer failures")
            .insert(failed, failure);
        f.coordinator
            .poll_bookmarks(&f.ctx, &mut HashMap::new())
            .await?;
        assert_eq!(
            f.cached("main"),
            Some(first),
            "keep the previous warm hash on failure"
        );
        assert_eq!(
            f.cached("other"),
            Some(good),
            "one failure must not cancel another bookmark"
        );
        f.failures.lock().expect("warmer failures").clear();
        f.coordinator
            .poll_bookmarks(&f.ctx, &mut HashMap::new())
            .await?;
        assert_eq!(f.cached("main"), Some(failed));
        assert_eq!(f.count(failed), 2);
        assert_eq!(f.count(good), 1);
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn observed_lag_grows_across_failed_retries_and_head_changes(
    fb: FacebookInit,
) -> Result<(), Error> {
    let f = Fixture::named(fb, "polling_observed_lag_retries").await?;
    let first = f.commit("warm").await?;
    let failed = f.commit("failed").await?;
    let later = f.commit("later").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    f.drop_log().await?;
    let mut unresolved = HashMap::new();
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    f.set("main", failed, BookmarkKind::Publishing).await?;
    f.failures
        .lock()
        .expect("warmer failures")
        .extend([(failed, WarmFailure::Error), (later, WarmFailure::Panic)]);
    let key = BookmarkKey::new("main")?;
    tokio::time::pause();
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    let observed_at = unresolved[&key];
    f.assert_observed_age(&unresolved, 0);
    tokio::time::advance(Duration::from_secs(7)).await;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    assert_eq!(unresolved[&key], observed_at);
    f.assert_observed_age(&unresolved, 7);

    f.set("main", later, BookmarkKind::Publishing).await?;
    tokio::time::advance(Duration::from_secs(5)).await;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    assert_eq!(
        unresolved[&key], observed_at,
        "new heads must not reset unresolved age",
    );
    f.assert_observed_age(&unresolved, 12);
    assert_eq!(f.cached("main"), Some(first));
    assert_eq!(f.count(failed), 2);
    assert_eq!(f.count(later), 1);
    f.failures.lock().expect("warmer failures").clear();
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    assert!(unresolved.is_empty());
    assert_eq!(f.cached("main"), Some(later));
    f.assert_observed_age(&unresolved, 0);
    tokio::time::resume();
    Ok(())
}

#[mononoke::fbinit_test]
async fn observed_lag_clears_on_catchup_without_warming_and_on_deletion(
    fb: FacebookInit,
) -> Result<(), Error> {
    let f = Fixture::named(fb, "polling_observed_lag_catchup").await?;
    let first = f.commit("warm").await?;
    let failed = f.commit("failed").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    f.set("main", first, BookmarkKind::Publishing).await?;
    f.drop_log().await?;
    let mut unresolved = HashMap::new();
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    f.failures
        .lock()
        .expect("warmer failures")
        .insert(failed, WarmFailure::Error);
    f.set("main", failed, BookmarkKind::Publishing).await?;
    tokio::time::pause();
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    tokio::time::advance(Duration::from_secs(6)).await;
    f.set("main", first, BookmarkKind::Publishing).await?;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    assert!(unresolved.is_empty());
    assert_eq!(
        f.count(first),
        1,
        "a matching cached hash/kind must not warm again",
    );
    f.assert_observed_age(&unresolved, 0);

    f.set("main", failed, BookmarkKind::Publishing).await?;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    assert_eq!(unresolved.len(), 1);
    tokio::time::advance(Duration::from_secs(4)).await;
    DeleteCurrent::query(
        &f.factory.metadata_db().write_connection,
        f.ctx.sql_query_telemetry(),
        &f.repo.repo_identity().id(),
        &"main".to_owned(),
    )
    .await?;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    assert!(unresolved.is_empty());
    assert_eq!(f.cached("main"), None);
    f.assert_observed_age(&unresolved, 0);
    tokio::time::resume();
    Ok(())
}

#[mononoke::fbinit_test]
async fn failed_snapshot_preserves_and_reports_known_unresolved_age(
    fb: FacebookInit,
) -> Result<(), Error> {
    let f = Fixture::named(fb, "polling_observed_lag_read_failure").await?;
    let first = f.commit("warm").await?;
    let failed = f.commit("failed").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    let mut unresolved = HashMap::new();
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    f.failures
        .lock()
        .expect("warmer failures")
        .insert(failed, WarmFailure::Error);
    f.set("main", failed, BookmarkKind::Publishing).await?;
    let key = BookmarkKey::new("main")?;
    tokio::time::pause();
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut unresolved)
        .await?;
    let observed_at = unresolved[&key];
    tokio::time::advance(Duration::from_secs(11)).await;
    DropBookmarks::query(
        &f.factory.metadata_db().write_connection,
        f.ctx.sql_query_telemetry(),
    )
    .await?;
    assert!(
        f.coordinator
            .poll_bookmarks(&f.ctx, &mut unresolved)
            .await
            .is_err()
    );
    assert_eq!(unresolved[&key], observed_at);
    assert_eq!(unresolved.len(), 1);
    assert_eq!(f.cached("main"), Some(first));
    f.assert_observed_age(&unresolved, 11);
    tokio::time::resume();
    Ok(())
}

#[mononoke::fbinit_test]
async fn sync_completes_when_the_bookmark_snapshot_read_fails(
    fb: FacebookInit,
) -> Result<(), Error> {
    let f = Fixture::new(fb).await?;
    let first = f.commit("before-read-failure").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    f.coordinator
        .poll_bookmarks(&f.ctx, &mut HashMap::new())
        .await?;
    DropBookmarks::query(
        &f.factory.metadata_db().write_connection,
        f.ctx.sql_query_telemetry(),
    )
    .await?;
    let cached = f.coordinator.bookmarks.clone();
    let sync_start = Arc::new(Notify::new());
    let sync_complete = Arc::new(Notify::new());
    let (terminate, receiver) = futures::channel::oneshot::channel();
    f.coordinator.spawn_polling(
        f.ctx.clone(),
        receiver,
        SyncSignals {
            start: sync_start.clone(),
            complete: sync_complete.clone(),
        },
    );
    let complete = sync_complete.notified();
    tokio::pin!(complete);
    complete.as_mut().enable();
    sync_start.notify_one();
    timeout(Duration::from_secs(5), complete).await?;
    assert_eq!(
        cached.with_read(|cache| cache
            .get(&BookmarkKey::new("main").expect("test key"))
            .and_then(|state| state
                .get(WarmerRequirement::AllKinds)
                .expect("all kinds tracked"))),
        Some(first),
        "a failed snapshot leaves the previously warmed hash unchanged"
    );
    terminate
        .send(())
        .expect("the coordinator survived the read failure");
    Ok(())
}

#[mononoke::fbinit_test]
async fn spawn_and_sync_need_no_log_subscription(fb: FacebookInit) -> Result<(), Error> {
    let f = Fixture::new(fb).await?;
    let first = f.commit("initial").await?;
    let next = f.commit("next").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    let cache = WarmBookmarksCache::new(
        &f.ctx,
        &f.repo.bookmarks_arc(),
        &f.repo.bookmark_update_log_arc(),
        &f.repo.repo_identity_arc(),
        &f.repo.repo_event_publisher_arc(),
        vec![Warmer {
            name: "OSS initialization".to_owned(),
            tags: vec![WarmerTag::Hg, WarmerTag::Git],
            is_warm: Box::new(|_, _| async { Ok(false) }.boxed()),
            warmer: Box::new(|_, _| async { Ok(()) }.boxed()),
        }],
        InitMode::Warm,
        WarmerRequirement::AllKinds,
    )
    .await?;
    assert_eq!(
        BookmarksCache::get(&cache, &f.ctx, &BookmarkKey::new("main")?).await?,
        Some(first)
    );
    BookmarksCache::sync(&cache, &f.ctx).await;
    f.drop_log().await?;
    f.set("main", next, BookmarkKind::Publishing).await?;
    timeout(Duration::from_secs(5), BookmarksCache::sync(&cache, &f.ctx)).await?;
    assert_eq!(
        BookmarksCache::get(&cache, &f.ctx, &BookmarkKey::new("main")?).await?,
        Some(next)
    );
    Ok(())
}

#[cfg(not(fbcode_build))]
async fn wait_for_cached(
    cached: &RwLock<HashMap<BookmarkKey, BookmarkState>>,
    key: &BookmarkKey,
    hash: ChangesetId,
) -> Result<(), Error> {
    timeout(
        Duration::from_secs(5),
        poll_fn(|cx| {
            if cached.with_read(|bookmarks| {
                bookmarks
                    .get(key)
                    .and_then(|state| state.get(WarmerRequirement::AllKinds).ok().flatten())
                    == Some(hash)
            }) {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }),
    )
    .await?;
    Ok(())
}

#[cfg(not(fbcode_build))]
#[mononoke::fbinit_test]
async fn next_poll_waits_for_all_warming_and_the_five_second_interval(
    fb: FacebookInit,
) -> Result<(), Error> {
    let f = Fixture::new(fb).await?;
    let first = f.commit("blocked").await?;
    let next = f.commit("after-blocked").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    f.drop_log().await?;
    let (entered, release) = f.gate(first);
    let cached = f.coordinator.bookmarks.clone();
    let calls = f.calls.clone();
    let (terminate, receiver) = futures::channel::oneshot::channel();
    f.coordinator.spawn_polling(
        f.ctx.clone(),
        receiver,
        SyncSignals {
            start: Arc::new(Notify::new()),
            complete: Arc::new(Notify::new()),
        },
    );
    timeout(Duration::from_secs(5), entered).await??;
    SetCurrentHash::query(
        &f.factory.metadata_db().write_connection,
        f.ctx.sql_query_telemetry(),
        &f.repo.repo_identity().id(),
        &"main".to_owned(),
        &next,
        &BookmarkKind::Publishing,
    )
    .await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(15)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.lock().expect("warmer calls").get(&first), Some(&1));
    assert!(
        !calls.lock().expect("warmer calls").contains_key(&next),
        "a poll must not run while the preceding batch is still warming"
    );
    tokio::time::resume();
    release
        .send(())
        .expect("the active batch is not cancelled by timer ticks");
    let key = BookmarkKey::new("main")?;
    wait_for_cached(&cached, &key, first).await?;
    tokio::task::yield_now().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert!(!calls.lock().expect("warmer calls").contains_key(&next));
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    wait_for_cached(&cached, &key, next).await?;
    assert_eq!(calls.lock().expect("warmer calls").get(&next), Some(&1));
    terminate
        .send(())
        .expect("the OSS coordinator is still running");
    Ok(())
}

#[mononoke::fbinit_test]
async fn termination_cancels_the_active_poll_without_publishing(
    fb: FacebookInit,
) -> Result<(), Error> {
    let f = Fixture::new(fb).await?;
    let first = f.commit("cancelled").await?;
    bookmark(&f.ctx, &f.repo, "main").set_to(first).await?;
    f.drop_log().await?;
    let (entered, mut release) = f.gate(first);
    let cached = f.coordinator.bookmarks.clone();
    let (terminate, receiver) = futures::channel::oneshot::channel();
    f.coordinator.spawn_polling(
        f.ctx.clone(),
        receiver,
        SyncSignals {
            start: Arc::new(Notify::new()),
            complete: Arc::new(Notify::new()),
        },
    );
    timeout(Duration::from_secs(5), entered).await??;
    terminate
        .send(())
        .expect("the OSS coordinator is still running");
    timeout(Duration::from_secs(5), release.closed()).await?;
    assert!(
        release.send(()).is_err(),
        "termination must drop active warming"
    );
    assert!(cached.with_read(HashMap::is_empty));
    Ok(())
}
