/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

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
    /// A build is in flight; `pending` is the handle it reports through.
    /// `stale` is the repo it replaces, served until the build finishes and
    /// kept if it fails.
    Building {
        stale: Option<Arc<R>>,
        pending: SharedBuild<R>,
    },
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
    /// completion handle, then wait on the shared handle like everyone else.
    Start(BuildCompletion<R>, SharedBuild<R>),
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
            SlotState::Empty
            | SlotState::Building { stale: None, .. }
            | SlotState::Failed { .. } => None,
            SlotState::Ready(repo)
            | SlotState::Building {
                stale: Some(repo), ..
            } => Some(Arc::clone(repo)),
        }
    }

    /// The claim this slot can answer without taking the lock. `None` when
    /// it needs a writer to start a build. A `fresh` caller wants a new build
    /// even of a built repo, so only an in-flight build or a held failure
    /// answers it.
    fn peek(&self, fresh: bool) -> Option<Claim<R>> {
        match &**self.state.load() {
            SlotState::Ready(repo)
            | SlotState::Building {
                stale: Some(repo), ..
            } if !fresh => Some(Claim::Ready(Arc::clone(repo))),
            SlotState::Building { pending, .. } => Some(Claim::Wait(pending.clone())),
            SlotState::Failed { error, retry_after } if Instant::now() < *retry_after => {
                Some(Claim::Failed(Arc::clone(error)))
            }
            // An elapsed failure is retryable, which needs a writer to claim,
            // exactly like `Empty`.
            SlotState::Ready(_) | SlotState::Empty | SlotState::Failed { .. } => None,
        }
    }

    /// Resolves the slot to the built repo, an in-flight build to wait on, or
    /// the obligation to run one. The build is not run here and needs no
    /// runtime: this decides *who* builds, not *how*.
    ///
    /// Exactly one caller is handed [`Claim::Start`] per build, and dropping
    /// its [`BuildCompletion`] without finishing publishes a failure, so a
    /// claim can never be silently abandoned.
    ///
    /// Single-flight holds **per slot, not per repo name**: `remove` followed
    /// by a re-`add` of the same name makes a *new* slot, so a build against
    /// the old one can still be in flight. The orphaned slot is unreachable and
    /// its result dropped, so this costs a duplicate build, never a wrong repo.
    /// A caller that evicts avoids even that by not removing a repo while
    /// anyone still waits on its build: the async-requests worker, which unloads
    /// an on-demand repo when its last request finishes, holds each request's
    /// reference until that request has seen the build complete.
    fn claim_build(self: &Arc<Self>, fresh: bool) -> Claim<R> {
        if let Some(claim) = self.peek(fresh) {
            return claim;
        }

        let transition = self.transition.lock();

        // Another writer may have started or finished a build since the peek
        // above.
        if let Some(claim) = self.peek(fresh) {
            return claim;
        }

        let stale = self.loaded();
        let (send_outcome, outcome) = oneshot::channel();
        let pending = outcome.shared();
        self.state.store(Arc::new(SlotState::Building {
            stale: stale.clone(),
            pending: pending.clone(),
        }));
        drop(transition);

        Claim::Start(
            BuildCompletion {
                repo_slot: Arc::clone(self),
                stale,
                send_outcome: Some(send_outcome),
            },
            pending,
        )
    }

    /// Publishes a finished build into this slot.
    ///
    /// Deliberately has no "was this repo removed?" check: this writes only
    /// slot interior, never the repo map, so a slot that `remove` or a
    /// replacing `add` detached is unreachable whatever is written to it. Keep
    /// it that way - writing back to the map from here would reintroduce the
    /// race `reload_if_present` exists to close.
    fn apply_outcome(&self, outcome: &BuildOutcome<R>, stale: Option<Arc<R>>) {
        let next = match (outcome, stale) {
            (Ok(repo), _) => SlotState::Ready(Arc::clone(repo)),
            // A failed rebuild keeps serving the repo it was meant to replace.
            (Err(_), Some(stale)) => SlotState::Ready(stale),
            // A repo that never builds would otherwise look slow rather than
            // broken. This crate has no stats dependency and should keep it
            // that way, so the failure is counted by the loader that owns the
            // build (`mononoke.app.lazy_repo_build_failed` in `mononoke_app`).
            (Err(error), None) => SlotState::Failed {
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
        self.build_inner(false, loader).await
    }

    /// A newly built repo, even if this slot already holds one: the eager
    /// counterpart of [`Self::get_or_build`], for callers that want the
    /// repo's current config applied now.
    ///
    /// The built repo keeps being served while its replacement builds, and
    /// stays if that build fails. A caller arriving while any build is in
    /// flight shares it rather than starting another.
    pub(crate) async fn build_now(
        self: &Arc<Self>,
        loader: Arc<dyn RepoLoader<R>>,
    ) -> Result<Arc<R>>
    where
        R: Send + Sync + 'static,
    {
        self.build_inner(true, loader).await
    }

    async fn build_inner(
        self: &Arc<Self>,
        fresh: bool,
        loader: Arc<dyn RepoLoader<R>>,
    ) -> Result<Arc<R>>
    where
        R: Send + Sync + 'static,
    {
        let pending = match self.claim_build(fresh) {
            Claim::Ready(repo) => return Ok(repo),
            Claim::Failed(error) => {
                return Err(anyhow!(
                    "Not retrying build of repo {} yet, it failed within the last {}s: {error:#}",
                    self.name,
                    FAILED_BUILD_TTL.as_secs()
                ));
            }
            Claim::Wait(pending) => pending,
            Claim::Start(completion, pending) => {
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
                let repo_name = self.name.clone();
                mononoke::spawn_task(async move {
                    completion.finish(loader.load(repo_name).await.map(Arc::new).map_err(Arc::new));
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
            // Unreachable while `BuildCompletion` is the only way to finish a
            // build, since it reports on `Drop` as well as on `finish` and so
            // cannot lose the sender. Kept as the honest answer if that ever
            // stops being true.
            Err(oneshot::Canceled) => Err(anyhow!("Build task for repo {} went away", self.name)),
        }
    }
}

/// The obligation that comes with winning a [`Claim::Start`]: publish the
/// build's result into the slot and hand it to every waiter.
///
/// Dropping it without calling [`BuildCompletion::finish`] publishes a failure
/// instead, so a build that panics, or is dropped by a shutting-down runtime,
/// cannot leave the slot stuck in [`SlotState::Building`] behind an
/// already-resolved handle - a state nothing recovers from, because the slot
/// never looks claimable again.
struct BuildCompletion<R> {
    repo_slot: Arc<RepoSlot<R>>,
    stale: Option<Arc<R>>,
    send_outcome: Option<oneshot::Sender<BuildOutcome<R>>>,
}

impl<R> BuildCompletion<R> {
    /// Publishes the result of the build. Takes `self` by value so the
    /// obligation is discharged exactly once.
    fn finish(mut self, outcome: BuildOutcome<R>) {
        self.publish(outcome);
    }

    /// Applies the outcome before sending, so a waiter woken by it always
    /// observes the slot in its final state rather than still `Building`.
    fn publish(&mut self, outcome: BuildOutcome<R>) {
        let Some(send_outcome) = self.send_outcome.take() else {
            return;
        };
        self.repo_slot.apply_outcome(&outcome, self.stale.take());
        let _ = send_outcome.send(outcome);
    }
}

impl<R> Drop for BuildCompletion<R> {
    fn drop(&mut self) {
        // `publish` would no-op anyway once `finish` has taken the sender. The
        // check is to not build an error message on every successful build.
        if self.send_outcome.is_none() {
            return;
        }
        let repo_name = self.repo_slot.name().to_string();
        self.publish(Err(Arc::new(anyhow!(
            "Build of repo {repo_name} ended without producing a result"
        ))));
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

    #[mononoke::test]
    async fn test_build_now_builds_an_empty_slot() {
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        let repo = repo_slot
            .build_now(Arc::new(TestLoader::Ready))
            .await
            .unwrap();
        assert_eq!(*repo, 42);
        assert_eq!(repo_slot.loaded().as_deref(), Some(&42));
    }

    #[mononoke::test]
    async fn test_build_now_serves_the_old_repo_until_the_new_one_is_built() {
        let gate = Arc::new(Gate::default());
        let repo_slot = Arc::new(RepoSlot::ready("foo".to_string(), Arc::new(100)));

        let rebuild = {
            let (repo_slot, gate) = (Arc::clone(&repo_slot), Arc::clone(&gate));
            mononoke::spawn_task(
                async move { repo_slot.build_now(Arc::new(GatedLoader(gate))).await },
            )
        };
        gate.started.notified().await;

        assert_eq!(
            repo_slot.loaded().as_deref(),
            Some(&100),
            "readers keep the old repo while its replacement builds"
        );
        let repo = repo_slot
            .get_or_build(Arc::new(TestLoader::Never))
            .await
            .unwrap();
        assert_eq!(*repo, 100, "a lazy caller must not wait on a rebuild");

        gate.release.notify_one();
        assert_eq!(*rebuild.await.unwrap().unwrap(), 42);
        assert_eq!(repo_slot.loaded().as_deref(), Some(&42));
    }

    #[mononoke::test]
    async fn test_a_failed_rebuild_keeps_the_old_repo_without_a_hold() {
        let repo_slot = Arc::new(RepoSlot::ready("foo".to_string(), Arc::new(100)));

        let err = repo_slot
            .build_now(Arc::new(TestLoader::Failed))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to build repo foo"),
            "unexpected error: {err:#}"
        );
        assert_eq!(repo_slot.loaded().as_deref(), Some(&100));

        // The old repo is still served, so there is no failure to hold.
        let repo = repo_slot
            .build_now(Arc::new(TestLoader::Ready))
            .await
            .unwrap();
        assert_eq!(*repo, 42);
    }

    #[mononoke::test]
    async fn test_a_panicking_rebuild_keeps_the_old_repo() {
        let repo_slot = Arc::new(RepoSlot::ready("foo".to_string(), Arc::new(100)));

        assert!(
            repo_slot
                .build_now(Arc::new(TestLoader::Panics))
                .await
                .is_err()
        );
        assert_eq!(repo_slot.loaded().as_deref(), Some(&100));
    }

    #[mononoke::test]
    async fn test_build_now_shares_an_in_flight_build() {
        let gate = Arc::new(Gate::default());
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        let first = call(&repo_slot, &gate);
        gate.started.notified().await;
        let eager = {
            let repo_slot = Arc::clone(&repo_slot);
            mononoke::spawn_task(
                async move { repo_slot.build_now(Arc::new(TestLoader::Never)).await },
            )
        };
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        gate.release.notify_one();

        assert_eq!(*first.await.unwrap().unwrap(), 42);
        assert_eq!(*eager.await.unwrap().unwrap(), 42);
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    }

    #[mononoke::test]
    async fn test_build_now_serves_a_held_failure() {
        let repo_slot = Arc::new(RepoSlot::empty("foo".to_string()));

        assert!(
            repo_slot
                .get_or_build(Arc::new(TestLoader::Failed))
                .await
                .is_err()
        );
        let err = repo_slot
            .build_now(Arc::new(TestLoader::Never))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Not retrying build of repo foo"),
            "unexpected error: {err:#}"
        );
    }

    struct PanicsOnLoad;

    impl RepoLoader<i32> for PanicsOnLoad {
        fn load(&self, repo_name: String) -> BoxFuture<'static, Result<i32>> {
            panic!("load of {repo_name} blew up before returning a future")
        }
    }

    #[mononoke::test]
    async fn test_a_loader_panicking_before_returning_a_future_finishes_the_build() {
        for old_repo in [None, Some(100)] {
            let repo_slot = Arc::new(match old_repo {
                None => RepoSlot::empty("foo".to_string()),
                Some(repo) => RepoSlot::ready("foo".to_string(), Arc::new(repo)),
            });

            assert!(repo_slot.build_now(Arc::new(PanicsOnLoad)).await.is_err());
            assert_eq!(repo_slot.loaded().as_deref(), old_repo.as_ref());

            repo_slot.expire_cached_failure();
            let repo = repo_slot
                .build_now(Arc::new(TestLoader::Ready))
                .await
                .expect("a loader panic must not leave the slot stuck building");
            assert_eq!(*repo, 42);
        }
    }
}
