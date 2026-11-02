/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::Shared;
use mononoke_macros::mononoke;
use parking_lot::Mutex;

use crate::RepoLoader;

/// How long a failed build is remembered before a caller is allowed to retry.
///
/// Without this a repo that fails deterministically, from a bad config or a
/// missing ACL, is rebuilt on every request for as long as traffic keeps
/// arriving. The window bounds that to one build per slot, while staying short
/// enough that a transient failure does not strand a repo for long.
const FAILED_BUILD_TTL: Duration = Duration::from_secs(10);

/// The result of one build, handed to every caller waiting on it.
///
/// The error is `Arc`-wrapped because `anyhow::Error` is not `Clone`.
type BuildOutcome<R> = Result<Arc<R>, Arc<Error>>;

/// A handle on an in-flight build, shared by every caller waiting on it.
type SharedBuild<R> = Shared<oneshot::Receiver<BuildOutcome<R>>>;

/// One repo's entry in [`crate::MononokeRepos`]: either the built repo, or
/// nothing yet.
///
/// # Experimental: use at your own risk
///
/// This type is part of the inexpensive-repos work (lazy repo loading), which
/// is still rolling out. Every repo `MononokeReposManager` holds is built
/// through a slot, eager or lazy, but what an unbuilt repo means for callers
/// is not settled, and the semantics here (single-flight, failed-build hold,
/// eviction) may still change.
///
/// If you build on `RepoSlot` or its unbuilt state, expect to follow those
/// changes, and give lmvasquezg a heads-up so the rollout knows about you.
///
/// Slots are `Arc`-shared and outlive any individual snapshot of the repo map,
/// so a caller that resolved a slot from an older snapshot still observes state
/// transitions made through a newer one.
pub struct RepoSlot<R> {
    name: String,
    /// Read on every repo lookup, so it is swapped rather than locked: serving
    /// a built repo is a read, and readers must not have to exclude each other
    /// to do it.
    state: ArcSwap<SlotState<R>>,
    /// Serializes writers: deciding who starts a build is a read-decide-write,
    /// which no atomic load expresses. Readers never take it.
    transition: Mutex<()>,
}

enum SlotState<R> {
    /// Assigned to this host, but not built.
    Empty,
    /// A build is in flight; this is the handle it reports through.
    Building(SharedBuild<R>),
    /// Built, and available to serve.
    Ready(Arc<R>),
    /// A build failed recently. Kept rather than dropped back to `Empty` so
    /// the next caller is served the failure instead of starting another
    /// build; `retry_after` is when that stops.
    Failed {
        error: Arc<Error>,
        retry_after: Instant,
    },
}

/// What [`RepoSlot::claim_build`] decided the caller should do.
enum Claim<R> {
    /// Already built.
    Ready(Arc<R>),
    /// Someone else is already building this repo.
    Wait(SharedBuild<R>),
    /// A recent build failed and the retry window has not elapsed.
    Failed(Arc<Error>),
    /// This caller won the race: run the build, report it through the
    /// sender, then wait on the shared handle like everyone else.
    Start(oneshot::Sender<BuildOutcome<R>>, SharedBuild<R>),
}

impl<R> RepoSlot<R> {
    /// A slot for a repo assigned to this service but not built.
    pub(crate) fn empty(name: String) -> Self {
        Self::in_state(name, SlotState::Empty)
    }

    /// A slot for a repo that is already built.
    pub(crate) fn ready(name: String, repo: Arc<R>) -> Self {
        Self::in_state(name, SlotState::Ready(repo))
    }

    fn in_state(name: String, state: SlotState<R>) -> Self {
        Self {
            name,
            state: ArcSwap::from_pointee(state),
            transition: Mutex::new(()),
        }
    }

    /// Which repo this slot is for. A slot outlives any one map snapshot, so it
    /// has to carry its own identity rather than rely on the key it was found
    /// under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The built repo, or `None` if this slot has not been built.
    pub fn loaded(&self) -> Option<Arc<R>> {
        match &**self.state.load() {
            SlotState::Empty | SlotState::Building(_) | SlotState::Failed { .. } => None,
            SlotState::Ready(repo) => Some(Arc::clone(repo)),
        }
    }

    /// Swaps in a newly built repo for the one this slot serves. `false`,
    /// without swapping, if the slot is not built: a build in flight would
    /// overwrite the swap with its own outcome.
    pub(crate) fn replace(&self, repo: Arc<R>) -> bool {
        let _transition = self.transition.lock();
        if !matches!(**self.state.load(), SlotState::Ready(_)) {
            return false;
        }
        self.state.store(Arc::new(SlotState::Ready(repo)));
        true
    }

    /// The claim this slot can answer without taking the lock. `None` when it
    /// is `Empty`, the one case that needs a writer.
    fn peek(&self) -> Option<Claim<R>> {
        match &**self.state.load() {
            SlotState::Ready(repo) => Some(Claim::Ready(Arc::clone(repo))),
            SlotState::Building(pending) => Some(Claim::Wait(pending.clone())),
            SlotState::Failed { error, retry_after } if Instant::now() < *retry_after => {
                Some(Claim::Failed(Arc::clone(error)))
            }
            // An elapsed failure is retryable, which needs a writer to claim,
            // exactly like `Empty`.
            SlotState::Empty | SlotState::Failed { .. } => None,
        }
    }

    /// Resolves the slot to the built repo, an in-flight build to wait on, or
    /// the obligation to run one. The build is not run here and needs no
    /// runtime: this decides *who* builds, not *how*.
    ///
    /// Exactly one caller is handed [`Claim::Start`] per build.
    ///
    /// Single-flight holds **per slot, not per repo name**: `remove` followed
    /// by a re-`add` of the same name makes a *new* slot, so a build against
    /// the old one can still be in flight. The orphaned slot is unreachable and
    /// its result dropped, so this costs a duplicate build, never a wrong repo.
    /// A caller that evicts avoids even that by not removing a repo while
    /// anyone still waits on its build: the async-requests worker, which unloads
    /// an on-demand repo when its last request finishes, holds each request's
    /// reference until that request has seen the build complete.
    fn claim_build(&self) -> Claim<R> {
        if let Some(claim) = self.peek() {
            return claim;
        }

        let transition = self.transition.lock();

        // Another writer may have started or finished a build since the peek
        // above.
        if let Some(claim) = self.peek() {
            return claim;
        }

        let (send_outcome, outcome) = oneshot::channel();
        let pending = outcome.shared();
        self.state
            .store(Arc::new(SlotState::Building(pending.clone())));
        drop(transition);

        Claim::Start(send_outcome, pending)
    }

    /// Publishes a finished build into this slot.
    ///
    /// Deliberately has no "was this repo removed?" check: this writes only
    /// slot interior, never the repo map, so a slot that `remove` or a
    /// replacing `add` detached is unreachable whatever is written to it. Keep
    /// it that way - writing back to the map from here would reintroduce the
    /// race `reload_if_present` exists to close.
    fn apply_outcome(&self, outcome: &BuildOutcome<R>) {
        let next = match outcome {
            Ok(repo) => SlotState::Ready(Arc::clone(repo)),
            // A repo that never builds would otherwise look slow rather than
            // broken. This crate has no stats dependency and should keep it
            // that way, so the failure is counted by the loader that owns the
            // build (`mononoke.app.lazy_repo_build_failed` in `mononoke_app`).
            Err(error) => SlotState::Failed {
                error: Arc::clone(error),
                retry_after: Instant::now() + FAILED_BUILD_TTL,
            },
        };
        let _transition = self.transition.lock();
        self.state.store(Arc::new(next));
    }

    /// Brings a cached failure's retry window forward to now, so a test can
    /// observe the retry without sleeping out the real one.
    #[cfg(test)]
    fn expire_cached_failure(&self) {
        let _transition = self.transition.lock();
        let SlotState::Failed { error, .. } = &**self.state.load() else {
            return;
        };
        self.state.store(Arc::new(SlotState::Failed {
            error: Arc::clone(error),
            retry_after: Instant::now(),
        }));
    }

    /// The built repo, loading it if this is the first caller to ask.
    pub(crate) async fn get_or_build(
        self: &Arc<Self>,
        loader: Arc<dyn RepoLoader<R>>,
    ) -> Result<Arc<R>>
    where
        R: Send + Sync + 'static,
    {
        let pending = match self.claim_build() {
            Claim::Ready(repo) => return Ok(repo),
            Claim::Failed(error) => {
                return Err(anyhow!(
                    "Not retrying build of repo {} yet, it failed within the last {}s: {error:#}",
                    self.name,
                    FAILED_BUILD_TTL.as_secs()
                ));
            }
            Claim::Wait(pending) => pending,
            Claim::Start(send_outcome, pending) => {
                // Detached rather than driven by the caller's future: a caller
                // that goes away mid-build (client disconnect, deadline) must
                // neither cancel the build nor force the next caller to restart
                // it. Builds run to tens of seconds, so restart-on-cancel would
                // livelock a repo behind any client timeout shorter than its
                // build.
                //
                // Unbounded here: at most one spawn per slot at a time, so
                // concurrent builds are bounded only by how many distinct
                // unbuilt repos callers ask for at once. The async-requests
                // worker is bounded by how many requests it runs at once, and
                // request-time loading by the loader `mononoke_app` installs.
                let repo_slot = Arc::clone(self);
                mononoke::spawn_task(async move {
                    let repo_name = repo_slot.name.clone();
                    // A panicking build must still publish an outcome: left
                    // `Building`, the slot would never be claimable again.
                    let outcome = AssertUnwindSafe(async move { loader.load(repo_name).await })
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| {
                            Err(anyhow!("Build of repo {} panicked", repo_slot.name))
                        })
                        .map(Arc::new)
                        .map_err(Arc::new);
                    // Applied before sending, so a woken waiter sees the final
                    // state rather than still `Building`.
                    repo_slot.apply_outcome(&outcome);
                    let _ = send_outcome.send(outcome);
                });
                pending
            }
        };

        self.wait_for_build(pending).await
    }

    async fn wait_for_build(&self, pending: SharedBuild<R>) -> Result<Arc<R>> {
        match pending.await {
            Ok(Ok(repo)) => Ok(repo),
            Ok(Err(error)) => Err(anyhow!("Failed to build repo {}: {error:#}", self.name)),
            // Only when the build task is dropped unfinished, which happens
            // only as the runtime shuts down.
            Err(oneshot::Canceled) => Err(anyhow!("Build task for repo {} went away", self.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use anyhow::bail;
    use futures::future::BoxFuture;
    use mononoke_macros::mononoke;
    use tokio::sync::Notify;
    use tokio::task::JoinHandle;

    use super::*;

    /// Builds `42`, but not until the test releases it, so the two concurrency
    /// tests can hold a build in flight instead of racing for one.
    #[derive(Default)]
    struct Gate {
        calls: AtomicUsize,
        started: Notify,
        release: Notify,
    }

    impl Gate {
        async fn build(self: Arc<Self>) -> Result<i32> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(42)
        }
    }

    struct GatedLoader(Arc<Gate>);

    impl RepoLoader<i32> for GatedLoader {
        fn load(&self, _repo_name: String) -> BoxFuture<'static, Result<i32>> {
            Arc::clone(&self.0).build().boxed()
        }
    }

    fn call(repo_slot: &Arc<RepoSlot<i32>>, gate: &Arc<Gate>) -> JoinHandle<Result<Arc<i32>>> {
        let repo_slot = Arc::clone(repo_slot);
        let loader = Arc::new(GatedLoader(Arc::clone(gate)));
        mononoke::spawn_task(async move { repo_slot.get_or_build(loader).await })
    }

    enum TestLoader {
        Ready,
        Failed,
        Panics,
        Never,
    }

    impl RepoLoader<i32> for TestLoader {
        fn load(&self, repo_name: String) -> BoxFuture<'static, Result<i32>> {
            match self {
                Self::Ready => async { Ok(42) }.boxed(),
                Self::Failed => async move { bail!("no config for {repo_name}") }.boxed(),
                Self::Panics => async move { panic!("build of {repo_name} blew up") }.boxed(),
                Self::Never => async move { panic!("must not build {repo_name}") }.boxed(),
            }
        }
    }

    /// Builds run on a detached task, so completion is observed by polling
    /// rather than by awaiting the caller that triggered it.
    async fn wait_until_loaded(repo_slot: &RepoSlot<i32>) -> Option<Arc<i32>> {
        for _ in 0..10_000 {
            if let Some(repo) = repo_slot.loaded() {
                return Some(repo);
            }
            tokio::task::yield_now().await;
        }
        None
    }

    #[mononoke::test]
    fn test_empty_slot_holds_no_repo() {
        let repo_slot: RepoSlot<i32> = RepoSlot::empty("foo".to_string());
        assert!(
            repo_slot.loaded().is_none(),
            "an unbuilt slot must look absent to every reader"
        );
    }

    #[mononoke::test]
    fn test_ready_slot_hands_out_its_repo() {
        let repo_slot = RepoSlot::ready("foo".to_string(), Arc::new(42));
        assert_eq!(repo_slot.loaded().as_deref(), Some(&42));
    }

    #[mononoke::test]
    fn test_reading_a_slot_shares_rather_than_copies() {
        let repo = Arc::new(42);
        let repo_slot = RepoSlot::ready("foo".to_string(), Arc::clone(&repo));

        // Two reads must hand back the same allocation, not clones of the repo:
        // a repo is expensive and callers rely on sharing one instance.
        let first = repo_slot.loaded().expect("slot was built");
        let second = repo_slot.loaded().expect("slot was built");
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first, &repo));
    }

    #[mononoke::test]
    async fn test_concurrent_callers_share_one_build() {
        let gate = Arc::new(Gate::default());
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        let first = call(&repo_slot, &gate);

        gate.started.notified().await;
        assert!(
            repo_slot.loaded().is_none(),
            "a repo must not be visible while its build is still running"
        );

        // Every later caller arrives while that build is in flight.
        let rest: Vec<_> = (0..9).map(|_| call(&repo_slot, &gate)).collect();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        gate.release.notify_one();

        for caller in std::iter::once(first).chain(rest) {
            assert_eq!(*caller.await.unwrap().unwrap(), 42);
        }
        assert_eq!(
            gate.calls.load(Ordering::SeqCst),
            1,
            "10 concurrent callers must produce exactly one build"
        );
    }

    #[mononoke::test]
    async fn test_build_survives_its_caller_being_cancelled() {
        let gate = Arc::new(Gate::default());
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        let caller = call(&repo_slot, &gate);
        gate.started.notified().await;

        // The only caller goes away mid-build, as a disconnecting client would.
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        gate.release.notify_one();

        assert_eq!(
            wait_until_loaded(&repo_slot).await.as_deref(),
            Some(&42),
            "the build must finish and apply its outcome even with no caller left waiting"
        );
        assert_eq!(
            gate.calls.load(Ordering::SeqCst),
            1,
            "the abandoned build must not be restarted"
        );
    }

    #[mononoke::test]
    async fn test_a_built_slot_is_served_without_rebuilding() {
        let repo_slot = Arc::new(RepoSlot::ready("foo".to_string(), Arc::new(100)));

        let repo = repo_slot
            .get_or_build(Arc::new(TestLoader::Never))
            .await
            .unwrap();
        assert_eq!(*repo, 100);
    }

    #[mononoke::test]
    async fn test_a_failed_build_is_reported_then_cached() {
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        let err = repo_slot
            .get_or_build(Arc::new(TestLoader::Failed))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to build repo foo"),
            "unexpected error: {err:#}"
        );
        assert!(repo_slot.loaded().is_none());

        // The next caller is served the failure rather than starting a second
        // build. `TestLoader::Never` panics if reached, so this pins that the build
        // does not run again, not merely that the call still errors.
        let err = repo_slot
            .get_or_build(Arc::new(TestLoader::Never))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Not retrying build of repo foo"),
            "unexpected error: {err:#}"
        );
    }

    #[mononoke::test]
    async fn test_a_cached_failure_stops_holding_the_slot_once_it_expires() {
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        assert!(
            repo_slot
                .get_or_build(Arc::new(TestLoader::Failed))
                .await
                .is_err()
        );
        repo_slot.expire_cached_failure();

        let repo = repo_slot
            .get_or_build(Arc::new(TestLoader::Ready))
            .await
            .expect("an expired failure must let the next caller build");
        assert_eq!(*repo, 42);
    }

    #[mononoke::test]
    async fn test_a_panicking_build_leaves_the_slot_retryable() {
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        let err = repo_slot
            .get_or_build(Arc::new(TestLoader::Panics))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to build repo foo"),
            "unexpected error: {err:#}"
        );

        // The point of the test: a dying build must release the slot. Left
        // mid-build it would wedge the repo for the life of the process.
        // Releasing it into the retry window rather than straight back to
        // `Empty` still satisfies that; what must not happen is a wedge that
        // outlives the window.
        repo_slot.expire_cached_failure();
        let repo = repo_slot
            .get_or_build(Arc::new(TestLoader::Ready))
            .await
            .expect("a panicking build must leave the slot retryable");
        assert_eq!(*repo, 42);
    }

    struct PanicsOnLoad;

    impl RepoLoader<i32> for PanicsOnLoad {
        fn load(&self, repo_name: String) -> BoxFuture<'static, Result<i32>> {
            panic!("load of {repo_name} blew up before returning a future")
        }
    }

    #[mononoke::test]
    async fn test_a_loader_panicking_before_returning_a_future_finishes_the_build() {
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        assert!(
            repo_slot
                .get_or_build(Arc::new(PanicsOnLoad))
                .await
                .is_err()
        );
        assert!(repo_slot.loaded().is_none());

        repo_slot.expire_cached_failure();
        let repo = repo_slot
            .get_or_build(Arc::new(TestLoader::Ready))
            .await
            .expect("a loader panic must not leave the slot stuck building");
        assert_eq!(*repo, 42);
    }

    #[mononoke::test]
    fn test_replace_swaps_only_a_built_repo() {
        let repo_slot = RepoSlot::ready("foo".to_string(), Arc::new(100));
        assert!(repo_slot.replace(Arc::new(42)));
        assert_eq!(repo_slot.loaded().as_deref(), Some(&42));

        let repo_slot: RepoSlot<i32> = RepoSlot::empty("foo".to_string());
        assert!(
            !repo_slot.replace(Arc::new(42)),
            "an unbuilt slot must not be filled by a rebuild"
        );
        assert!(repo_slot.loaded().is_none());
    }
}
