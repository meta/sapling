/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::error::Error as StdError;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use async_trait::async_trait;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivationError;
use derived_data_manager::DerivedDataManager;
use derived_data_manager::SharedDerivationError;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use futures_retry::retry;
use mercurial_derivation::RootHgAugmentedManifestId;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use repo_blobstore::RepoBlobstore;
use thiserror::Error;
use throttledblob::ThrottleOptions;
use throttledblob::ThrottledBlob;
use tokio_util::task::AbortOnDropHandle;
use topo_sort::TopoSortedDagTraversal;
use tracing::warn;
use wait_for_replication::WaitForReplication;

/// How many derivations a backfill runs at once. A value below 1 counts as 1.
const JK_CONCURRENCY: &str = "scm/mononoke:augmented_manifest_backfill_concurrency";

/// Evaluated with the repo name as the switch value. While it is true, a
/// backfill of that repo starts no new derivation and lists no new commits,
/// and the derivations already running finish.
const JK_PAUSED: &str = "scm/mononoke:augmented_manifest_backfill_paused";

/// How often a backfill re-reads its switches while it waits.
const SWITCH_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// How often a commit that fails to load or derive is tried, and how long to
/// wait before the first retry. Each later wait is twice as long.
const RETRIES: Retries = Retries {
    attempts: 4,
    first_delay: Duration::from_secs(1),
};

/// What deriving a list of commits did with one of them.
#[derive(Debug)]
pub enum CommitOutcome {
    /// The commit already had an augmented manifest.
    AlreadyDerived,
    /// The commit was derived. `count` is the number of commits this
    /// derivation derived: the commit itself and any of its ancestors that
    /// lacked augmented manifests. It is zero if another derivation got there
    /// first. After a retry it counts only what the last attempt derived.
    Derived { count: u64 },
    /// The commit was not derived.
    Failed(Failure),
}

/// Why a commit was not derived.
#[derive(Debug, Error)]
pub enum Failure {
    /// Loading or deriving the commit failed on every attempt. This is the
    /// error of the last attempt.
    #[error(transparent)]
    Error(#[from] anyhow::Error),
    /// A parent in the same list failed, so the commit was not tried.
    #[error("parent {parent} was not derived")]
    ParentFailed { parent: ChangesetId },
    /// The repo config blocks deriving `derived_data_type` for `commit`: the
    /// commit itself, or one of its ancestors.
    #[error("derivation of {derived_data_type} for {commit} is blocked in the repo config")]
    Blocked {
        derived_data_type: &'static str,
        commit: ChangesetId,
    },
}

#[derive(Clone, Copy, Debug)]
struct Retries {
    attempts: usize,
    first_delay: Duration,
}

/// Waits while the storage that derivation writes to lags behind.
#[async_trait]
pub trait ReplicationWait: Send + Sync {
    /// Returns once replication lag is below the limit.
    async fn wait_for_replication(&self) -> Result<()>;
}

#[async_trait]
impl ReplicationWait for WaitForReplication {
    async fn wait_for_replication(&self) -> Result<()> {
        WaitForReplication::wait_for_replication(self).await
    }
}

/// Derives augmented manifests, and the ACL manifests they depend on, for the
/// commits of one repo. Derivation is local: it never goes through the derived
/// data service.
pub struct RepoBackfill {
    manager: DerivedDataManager,
    replication: Arc<dyn ReplicationWait>,
    retries: Retries,
}

impl RepoBackfill {
    /// All the derivations of this backfill share one blobstore throttle with
    /// the limits of `throttle`. The throttle sits above the repo's caches, so
    /// reads served from the cache count towards the read limit too. Before
    /// starting more derivations, the backfill waits on `replication`.
    pub fn new(
        manager: DerivedDataManager,
        throttle: ThrottleOptions,
        replication: Arc<dyn ReplicationWait>,
    ) -> Self {
        Self {
            manager: throttled(manager, throttle),
            replication,
            retries: RETRIES,
        }
    }

    /// Returns once the repo's backfill is not paused, re-reading the pause
    /// switch every few seconds.
    pub(crate) async fn wait_while_paused(&self) {
        while is_paused(self.manager.repo_name()) {
            tokio::time::sleep(SWITCH_RECHECK_INTERVAL).await;
        }
    }

    /// Derives the commits of `commits`, which maps each commit to its parents,
    /// and returns what was done with each one.
    ///
    /// Commits that already have augmented manifests are skipped. The
    /// backfill's switches set how many commits are derived at once, and
    /// whether the repo is paused, and a commit waits only for
    /// its parents in the list, so a parent in the list is derived once rather
    /// than by each of its children. Commits outside the list are not tracked:
    /// two commits that share an underived parent outside the list may both
    /// derive it, and a commit that reaches a listed ancestor only through a
    /// commit outside the list does not wait for it, so that ancestor may be
    /// derived both on its own and by its descendant.
    ///
    /// Each commit fails on its own. A commit that fails to load or derive is
    /// retried with backoff, and then recorded as failed with its error. A
    /// commit whose parent in the list failed is recorded as failed without
    /// being tried. A commit whose derivation the repo config blocks, or whose
    /// derivation reaches such an ancestor, is recorded as blocked without a
    /// retry. The call itself fails only if the repo cannot derive augmented
    /// manifests, if it cannot check which commits are derived, if it cannot
    /// check the replication lag, or if the parents form a cycle.
    pub async fn derive_commits(
        &self,
        ctx: &CoreContext,
        mut commits: HashMap<ChangesetId, Vec<ChangesetId>>,
    ) -> Result<HashMap<ChangesetId, CommitOutcome>> {
        ensure!(
            self.manager
                .config()
                .types
                .contains(&RootHgAugmentedManifestId::VARIANT),
            "Augmented manifests are not enabled for derivation in this repo",
        );

        let derived = self
            .manager
            .fetch_derived_batch::<RootHgAugmentedManifestId>(
                ctx,
                commits.keys().copied().collect(),
                None,
            )
            .await
            .context("Failed to check which commits have augmented manifests")?;
        commits.retain(|cs_id, _| !derived.contains_key(cs_id));

        let results = derive_parents_first(
            &commits,
            self.manager.repo_name(),
            self.replication.as_ref(),
            |cs_id| {
                with_retries(self.retries, cs_id, move || {
                    self.try_derive_commit(ctx, cs_id)
                })
            },
        )
        .await?;

        Ok(derived
            .into_keys()
            .map(|cs_id| (cs_id, CommitOutcome::AlreadyDerived))
            .chain(results.into_iter().map(|(cs_id, result)| {
                let outcome = match result {
                    Ok(count) => CommitOutcome::Derived { count },
                    Err(failure) => CommitOutcome::Failed(failure),
                };
                (cs_id, outcome)
            }))
            .collect())
    }

    /// Derives `cs_id` once, on a task of its own. Deriving a commit that has
    /// an Hg changeset does its CPU work on the task that polls it, so without
    /// the task, derivations running at once would share one thread.
    ///
    /// Where ancestors were derived out of order, `derive_heads` can derive an
    /// ancestor that is already derived. The default blobstore put behaviour,
    /// which does not overwrite existing keys, keeps what is stored.
    async fn try_derive_commit(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<u64, Failure> {
        let manager = self.manager.clone();
        let ctx = ctx.clone();
        AbortOnDropHandle::new(mononoke::spawn_task(async move {
            manager
                .derive_heads::<RootHgAugmentedManifestId>(ctx, vec![cs_id], None, None)
                .await
                .map_err(|error| {
                    blocked_derivation(&error).unwrap_or_else(|| {
                        anyhow::Error::from(error)
                            .context(format!(
                                "Failed to derive the augmented manifest of {cs_id}"
                            ))
                            .into()
                    })
                })
        }))
        .await
        .with_context(|| format!("The task deriving {cs_id} panicked"))?
    }
}

/// The failure to record if `error`, or an error it was caused by, is the repo
/// config blocking a derivation.
fn blocked_derivation(error: &SharedDerivationError) -> Option<Failure> {
    let mut next: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(error) = next {
        let derivation_error = error
            .downcast_ref::<SharedDerivationError>()
            .map(|shared| shared.inner())
            .or_else(|| error.downcast_ref::<DerivationError>());
        next = match derivation_error {
            Some(DerivationError::Blocked(derived_data_type, commit)) => {
                return Some(Failure::Blocked {
                    derived_data_type,
                    commit: *commit,
                });
            }
            // The source of a transparent error skips the error it wraps.
            Some(DerivationError::Error(inner)) => Some(&**inner),
            _ => error.source(),
        };
    }
    None
}

/// Returns `manager` with its blobstore throttled to the limits of
/// `throttle`, unless `throttle` sets no limit.
fn throttled(manager: DerivedDataManager, throttle: ThrottleOptions) -> DerivedDataManager {
    if !throttle.has_throttle() {
        return manager;
    }
    let blobstore = RepoBlobstore::new_with_wrapped_inner_blobstore(
        manager.repo_blobstore().clone(),
        |inner| Arc::new(ThrottledBlob::new(inner, throttle)),
    );
    manager.with_replaced_blobstore(blobstore)
}

fn is_paused(repo_name: &str) -> bool {
    justknobs::eval(JK_PAUSED, None, Some(repo_name))
}

/// How many derivations may run at once in `repo_name`: none while its
/// backfill is paused.
fn concurrency_limit(repo_name: &str) -> usize {
    if is_paused(repo_name) {
        return 0;
    }
    usize::try_from(justknobs::get(JK_CONCURRENCY, None).max(1)).unwrap_or(usize::MAX)
}

/// Runs `attempt`, which derives `cs_id`, until it succeeds, it fails for a
/// reason other than an error, or `retries.attempts` attempts have failed,
/// waiting longer before each retry. Each failed attempt is logged.
async fn with_retries<F, Fut>(
    retries: Retries,
    cs_id: ChangesetId,
    mut attempt: F,
) -> Result<u64, Failure>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<u64, Failure>>,
{
    let (count, _attempts) = retry(|_| attempt(), retries.first_delay)
        .binary_exponential_backoff()
        .max_attempts(retries.attempts)
        .retry_if(|_, failure: &Failure| matches!(failure, Failure::Error(_)))
        .inspect_err(|attempt, failure: &Failure| {
            warn!(
                "Attempt {attempt} of {} to derive {cs_id} failed: {failure:#}",
                retries.attempts,
            );
        })
        .await?;
    Ok(count)
}

/// Derives each commit of `commits`, which maps each commit to its parents,
/// with `derive`, and returns what each derivation returned. A commit starts
/// only once its parents in `commits` have finished. A commit whose parent in
/// `commits` failed is recorded as failed without calling `derive`. Parents
/// outside `commits` are ignored.
///
/// How many derivations run at once follows the switches of `repo_name`,
/// re-read whenever a derivation finishes and every few seconds. A lower
/// limit, or a pause, stops new derivations from starting until the running
/// ones are below it; running ones are never stopped. Before starting more
/// derivations, it waits on `replication`, and fails if the lag cannot be
/// checked. Derivations already started go on during the wait, and the
/// switches are read again once it is over.
async fn derive_parents_first<F, Fut>(
    commits: &HashMap<ChangesetId, Vec<ChangesetId>>,
    repo_name: &str,
    replication: &dyn ReplicationWait,
    derive: F,
) -> Result<HashMap<ChangesetId, Result<u64, Failure>>>
where
    F: Fn(ChangesetId) -> Fut,
    Fut: Future<Output = Result<u64, Failure>>,
{
    let mut dag = TopoSortedDagTraversal::new(
        commits
            .iter()
            .map(|(cs_id, parents)| {
                let parents = parents
                    .iter()
                    .filter(|parent| commits.contains_key(*parent))
                    .copied()
                    .collect();
                (*cs_id, parents)
            })
            .collect(),
    );

    let mut results = HashMap::with_capacity(commits.len());
    let mut running = FuturesUnordered::new();
    loop {
        let mut limit = concurrency_limit(repo_name);
        if running.len() < limit && !dag.is_empty() {
            // Keep polling the running derivations while waiting: a pushed
            // derivation starts only when it is first polled.
            let mut wait = pin!(replication.wait_for_replication());
            loop {
                tokio::select! {
                    waited = &mut wait => {
                        waited.context("Failed to wait for replication before deriving")?;
                        break;
                    }
                    Some((cs_id, result)) = running.next() => {
                        results.insert(cs_id, result);
                        dag.visited(cs_id);
                    }
                }
            }
            limit = concurrency_limit(repo_name);
        }

        // No await from reading the limit to pushing the derivations, so
        // that they start under the switches as just read. Without a wait
        // above, the limit leaves no room or nothing is ready, so derivations
        // never start without a check of the lag.
        while running.len() < limit {
            let Some(cs_id) = dag.drain(1).next() else {
                break;
            };
            let failed_parent = commits[&cs_id]
                .iter()
                .find(|parent| matches!(results.get(*parent), Some(Err(_))));
            match failed_parent {
                Some(parent) => {
                    let failure = Failure::ParentFailed { parent: *parent };
                    results.insert(cs_id, Err(failure));
                    dag.visited(cs_id);
                }
                None => running.push(derive(cs_id).map(move |result| (cs_id, result))),
            }
        }

        // With nothing running, ready commits are left only while paused.
        if running.is_empty() && dag.is_empty() {
            break;
        }
        tokio::select! {
            Some((cs_id, result)) = running.next() => {
                results.insert(cs_id, result);
                dag.visited(cs_id);
            }
            _ = tokio::time::sleep(SWITCH_RECHECK_INTERVAL) => {}
        }
    }

    ensure!(
        results.len() == commits.len(),
        "{} of {} commits were never derived, because their parents form a cycle",
        commits.len() - results.len(),
        commits.len(),
    );
    Ok(results)
}

/// A replication wait that never waits.
#[cfg(test)]
pub(crate) struct NoReplicationLag;

#[cfg(test)]
#[async_trait]
impl ReplicationWait for NoReplicationLag {
    async fn wait_for_replication(&self) -> Result<()> {
        Ok(())
    }
}

/// Sets the backfill's switches for the rest of a test.
#[cfg(test)]
pub(crate) fn set_switches(concurrency: i64, paused: bool) {
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::override_just_knobs;

    override_just_knobs(JustKnobsInMemory::new(HashMap::from([
        (JK_CONCURRENCY.to_string(), KnobVal::Int(concurrency)),
        (JK_PAUSED.to_string(), KnobVal::Bool(paused)),
    ])));
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::num::NonZeroU32;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use assert_matches::assert_matches;
    use blobstore::KeyedBlobstore;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bonsai_hg_mapping::BonsaiHgMappingEntry;
    use bonsai_hg_mapping::BonsaiHgMappingRef;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use mercurial_types::HgChangesetId;
    use mononoke_types::BlobstoreBytes;
    use mononoke_types::DerivableType;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tokio::time::Instant;

    use super::*;

    #[facet::container]
    struct TestRepo(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        CommitGraph,
        dyn CommitGraphWriter,
        FilestoreConfig,
        RepoBlobstore,
        RepoDerivedData,
        RepoIdentity,
    );

    /// Enough to derive every commit of a test at once.
    const CONCURRENCY: i64 = 10;

    /// The repo name the switches are read for. Switch overrides in tests
    /// ignore it.
    const REPO: &str = "repo";

    /// A repo with one commit that has an augmented manifest, standing in for
    /// the public commit that drafts are based on.
    async fn repo_with_derived_base(
        fb: FacebookInit,
    ) -> Result<(CoreContext, TestRepo, ChangesetId)> {
        let (ctx, _factory, repo, base) = factory_with_derived_base(fb).await?;
        Ok((ctx, repo, base))
    }

    /// Like `repo_with_derived_base`, with the factory that built the repo.
    async fn factory_with_derived_base(
        fb: FacebookInit,
    ) -> Result<(CoreContext, TestRepoFactory, TestRepo, ChangesetId)> {
        let ctx = CoreContext::test_mock(fb);
        let factory = TestRepoFactory::new(fb)?;
        let repo: TestRepo = factory.build().await?;
        let base = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file", "base")
            .commit()
            .await?;
        repo.repo_derived_data()
            .manager()
            .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![base], None, None)
            .await?;
        Ok((ctx, factory, repo, base))
    }

    /// Another handle on the repo that `factory` built, sharing its storage,
    /// whose config blocks the derivation of every type for `blocked`.
    async fn with_derivation_blocked(
        factory: &mut TestRepoFactory,
        blocked: ChangesetId,
    ) -> Result<TestRepo> {
        factory
            .with_config_override(|config| {
                config
                    .derived_data_config
                    .blocked_derivation
                    .insert(blocked, None);
            })
            .build()
            .await
            .map_err(Into::into)
    }

    async fn draft(
        ctx: &CoreContext,
        repo: &TestRepo,
        parent: ChangesetId,
        content: &str,
    ) -> Result<ChangesetId> {
        CreateCommitContext::new(ctx, repo, vec![parent])
            .add_file("file", content)
            .commit()
            .await
    }

    /// A backfill that retries failures without waiting, so that tests of
    /// failures on a test repo stay fast.
    fn backfill(repo: &TestRepo) -> RepoBackfill {
        RepoBackfill {
            retries: Retries {
                first_delay: Duration::ZERO,
                ..RETRIES
            },
            ..RepoBackfill::new(
                repo.repo_derived_data().manager().clone(),
                ThrottleOptions::default(),
                Arc::new(NoReplicationLag),
            )
        }
    }

    async fn has_augmented_manifest(
        ctx: &CoreContext,
        repo: &TestRepo,
        cs_id: ChangesetId,
    ) -> Result<bool> {
        Ok(repo
            .repo_derived_data()
            .manager()
            .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
            .await?
            .is_some())
    }

    fn commit_id(byte: u8) -> ChangesetId {
        ChangesetId::from_bytes([byte; 32]).expect("32 bytes should make a changeset id")
    }

    #[mononoke::fbinit_test]
    async fn derives_a_stack_parents_first(fb: FacebookInit) -> Result<()> {
        // Given a stack of two drafts on a commit that has an augmented manifest.
        let (ctx, repo, base) = repo_with_derived_base(fb).await?;
        let bottom = draft(&ctx, &repo, base, "bottom").await?;
        let top = draft(&ctx, &repo, bottom, "top").await?;

        // When the backfill derives the stack.
        let outcomes = backfill(&repo)
            .derive_commits(
                &ctx,
                HashMap::from([(bottom, vec![base]), (top, vec![bottom])]),
            )
            .await?;

        // Then each draft's derivation derived only that draft, which means the
        // bottom was derived before the top.
        assert_eq!(outcomes.len(), 2, "both drafts should have an outcome");
        assert_matches!(outcomes[&bottom], CommitOutcome::Derived { count: 1 });
        assert_matches!(outcomes[&top], CommitOutcome::Derived { count: 1 });
        assert!(
            has_augmented_manifest(&ctx, &repo, top).await?,
            "the top of the stack should have an augmented manifest",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn skips_commits_that_are_already_derived(fb: FacebookInit) -> Result<()> {
        // Given a commit that has an augmented manifest, and a draft on it.
        let (ctx, repo, base) = repo_with_derived_base(fb).await?;
        let underived = draft(&ctx, &repo, base, "underived").await?;

        // When the backfill derives both.
        let outcomes = backfill(&repo)
            .derive_commits(
                &ctx,
                HashMap::from([(base, vec![]), (underived, vec![base])]),
            )
            .await?;

        // Then only the draft was derived.
        assert_eq!(outcomes.len(), 2, "both commits should have an outcome");
        assert_matches!(outcomes[&base], CommitOutcome::AlreadyDerived);
        assert_matches!(outcomes[&underived], CommitOutcome::Derived { count: 1 });
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn derives_underived_ancestors_outside_the_list(fb: FacebookInit) -> Result<()> {
        // Given a stack of two drafts that lack augmented manifests, of which
        // only the top is in the list.
        let (ctx, repo, base) = repo_with_derived_base(fb).await?;
        let bottom = draft(&ctx, &repo, base, "bottom").await?;
        let top = draft(&ctx, &repo, bottom, "top").await?;

        // When the backfill derives the top.
        let outcomes = backfill(&repo)
            .derive_commits(&ctx, HashMap::from([(top, vec![bottom])]))
            .await?;

        // Then deriving the top also derived the bottom.
        assert_eq!(outcomes.len(), 1, "the top should have an outcome");
        assert_matches!(outcomes[&top], CommitOutcome::Derived { count: 2 });
        assert!(
            has_augmented_manifest(&ctx, &repo, bottom).await?,
            "the bottom of the stack should have an augmented manifest",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn records_a_draft_whose_hg_changeset_is_missing(fb: FacebookInit) -> Result<()> {
        // Given a draft mapped to an Hg changeset whose blob is missing, and
        // another draft.
        let (ctx, repo, base) = repo_with_derived_base(fb).await?;
        let broken = draft(&ctx, &repo, base, "broken").await?;
        repo.bonsai_hg_mapping()
            .add(
                &ctx,
                BonsaiHgMappingEntry {
                    hg_cs_id: HgChangesetId::from_bytes(&[7; 20])?,
                    bcs_id: broken,
                },
            )
            .await?;
        let healthy = draft(&ctx, &repo, base, "healthy").await?;

        // When the backfill derives both.
        let outcomes = backfill(&repo)
            .derive_commits(
                &ctx,
                HashMap::from([(broken, vec![base]), (healthy, vec![base])]),
            )
            .await?;

        // Then the broken draft is recorded with the missing blob, and the
        // other draft is derived.
        assert_matches!(
            &outcomes[&broken],
            CommitOutcome::Failed(Failure::Error(error))
                if format!("{error:#}").contains("Blob is missing: hgchangeset"),
            "the broken draft should fail because its Hg changeset is missing",
        );
        assert_matches!(outcomes[&healthy], CommitOutcome::Derived { count: 1 });
        assert!(
            !has_augmented_manifest(&ctx, &repo, broken).await?,
            "the broken draft should not have an augmented manifest",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn records_a_commit_that_is_not_in_the_commit_graph(fb: FacebookInit) -> Result<()> {
        // Given a commit that is not in the repo's commit graph, and a draft.
        let (ctx, repo, base) = repo_with_derived_base(fb).await?;
        let unknown = commit_id(1);
        let healthy = draft(&ctx, &repo, base, "healthy").await?;

        // When the backfill derives both.
        let outcomes = backfill(&repo)
            .derive_commits(
                &ctx,
                HashMap::from([(unknown, vec![base]), (healthy, vec![base])]),
            )
            .await?;

        // Then the unknown commit is recorded with its error, and the draft is
        // derived.
        assert_matches!(
            outcomes[&unknown],
            CommitOutcome::Failed(Failure::Error(_)),
            "a commit missing from the commit graph should fail with its error",
        );
        assert_matches!(outcomes[&healthy], CommitOutcome::Derived { count: 1 });
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn records_a_commit_blocked_in_config_as_blocked(fb: FacebookInit) -> Result<()> {
        // Given a draft whose derivation the repo config blocks, a child of
        // it, and another draft.
        let (ctx, mut factory, repo, base) = factory_with_derived_base(fb).await?;
        let blocked = draft(&ctx, &repo, base, "blocked").await?;
        let child = draft(&ctx, &repo, blocked, "child").await?;
        let other = draft(&ctx, &repo, base, "other").await?;
        let repo = with_derivation_blocked(&mut factory, blocked).await?;

        // When the backfill derives the three.
        let outcomes = backfill(&repo)
            .derive_commits(
                &ctx,
                HashMap::from([
                    (blocked, vec![base]),
                    (child, vec![blocked]),
                    (other, vec![base]),
                ]),
            )
            .await?;

        // Then the blocked draft is recorded as blocked rather than with an
        // error, its child is recorded through its parent without being
        // tried, and the other draft is derived.
        assert_matches!(
            outcomes[&blocked],
            CommitOutcome::Failed(Failure::Blocked { commit, .. }) if commit == blocked,
            "the blocked draft should be recorded as blocked",
        );
        assert_matches!(
            outcomes[&child],
            CommitOutcome::Failed(Failure::ParentFailed { parent }) if parent == blocked
        );
        assert_matches!(outcomes[&other], CommitOutcome::Derived { count: 1 });
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn records_a_draft_whose_derivation_reaches_a_blocked_ancestor(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a stack of three drafts, the repo config blocking the
        // derivation of the bottom one, and another draft.
        let (ctx, mut factory, repo, base) = factory_with_derived_base(fb).await?;
        let bottom = draft(&ctx, &repo, base, "bottom").await?;
        let middle = draft(&ctx, &repo, bottom, "middle").await?;
        let top = draft(&ctx, &repo, middle, "top").await?;
        let other = draft(&ctx, &repo, base, "other").await?;
        let repo = with_derivation_blocked(&mut factory, bottom).await?;

        // When the backfill derives the top and the other draft.
        let outcomes = backfill(&repo)
            .derive_commits(
                &ctx,
                HashMap::from([(top, vec![middle]), (other, vec![base])]),
            )
            .await?;

        // Then the top is recorded as blocked at the bottom of its stack, and
        // nothing in its stack is derived, while the other draft is derived.
        assert_matches!(
            outcomes[&top],
            CommitOutcome::Failed(Failure::Blocked { commit, .. }) if commit == bottom,
            "the top should be recorded as blocked at the bottom of its stack",
        );
        assert_matches!(outcomes[&other], CommitOutcome::Derived { count: 1 });
        assert!(
            !has_augmented_manifest(&ctx, &repo, middle).await?,
            "no commit of the stack should be derived",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn fails_when_augmented_manifests_cannot_be_derived(fb: FacebookInit) -> Result<()> {
        // Given a repo where augmented manifests can be read but are not
        // enabled for derivation, and a commit that lacks one.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = TestRepoFactory::new(fb)?
            .with_config_override(|config| {
                let derived_data = &mut config.derived_data_config;
                derived_data
                    .get_active_config_mut()
                    .expect("the test repo should have an enabled derived data config")
                    .types
                    .remove(&DerivableType::HgAugmentedManifests);
                derived_data
                    .extra_types_available_for_read
                    .insert(DerivableType::HgAugmentedManifests);
            })
            .build()
            .await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file", "root")
            .commit()
            .await?;

        // When the backfill derives it.
        let result = backfill(&repo)
            .derive_commits(&ctx, HashMap::from([(root, vec![])]))
            .await;

        // Then the call fails, rather than recording every commit as failed.
        assert!(
            result.is_err(),
            "a repo that cannot derive augmented manifests should fail the call",
        );
        Ok(())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Step {
        Started(ChangesetId),
        Finished(ChangesetId),
        /// Something the test did while derivations ran.
        Note(&'static str),
    }

    /// The most derivations running at the same time in `steps`.
    fn most_running(steps: &[Step]) -> usize {
        steps
            .iter()
            .scan(0, |running, step| {
                *running = match step {
                    Step::Started(_) => *running + 1,
                    Step::Finished(_) => *running - 1,
                    Step::Note(_) => *running,
                };
                Some(*running)
            })
            .max()
            .unwrap_or(0)
    }

    /// How many derivations are running at the end of `steps`.
    fn running_after(steps: &[Step]) -> usize {
        let started = steps
            .iter()
            .filter(|s| matches!(s, Step::Started(_)))
            .count();
        let finished = steps
            .iter()
            .filter(|s| matches!(s, Step::Finished(_)))
            .count();
        started - finished
    }

    /// Stands in for derivation. It records when each commit starts and
    /// finishes, and yields in between, so that derivations running at once
    /// overlap. Commits in `failing` fail.
    #[derive(Default)]
    struct FakeDerivation {
        steps: RefCell<Vec<Step>>,
        failing: Vec<ChangesetId>,
        /// Commits whose derivation takes ten seconds.
        slow: Vec<ChangesetId>,
    }

    impl FakeDerivation {
        async fn derive(&self, cs_id: ChangesetId) -> Result<u64, Failure> {
            self.steps.borrow_mut().push(Step::Started(cs_id));
            if self.slow.contains(&cs_id) {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            self.steps.borrow_mut().push(Step::Finished(cs_id));
            if self.failing.contains(&cs_id) {
                return Err(anyhow::anyhow!("{cs_id} is broken").into());
            }
            Ok(1)
        }

        fn note(&self, note: &'static str) {
            self.steps.borrow_mut().push(Step::Note(note));
        }
    }

    /// A replication wait that, from its call numbered `lags_from` on
    /// (counting from 0), waits while `lagging` is set. It counts its calls.
    #[derive(Default)]
    struct FakeLag {
        lagging: AtomicBool,
        lags_from: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ReplicationWait for FakeLag {
        async fn wait_for_replication(&self) -> Result<()> {
            if self.calls.fetch_add(1, Ordering::Relaxed) < self.lags_from {
                return Ok(());
            }
            while self.lagging.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok(())
        }
    }

    /// A replication wait that cannot check the lag.
    struct BrokenLag;

    #[async_trait]
    impl ReplicationWait for BrokenLag {
        async fn wait_for_replication(&self) -> Result<()> {
            Err(anyhow::anyhow!("the lag cannot be read"))
        }
    }

    #[mononoke::test]
    async fn starts_a_commit_only_after_its_parents_finish() -> Result<()> {
        set_switches(CONCURRENCY, false);
        // Given a commit with two children, one of which has a child of its own.
        let (root, left, left_child, right) =
            (commit_id(1), commit_id(2), commit_id(3), commit_id(4));
        let commits = HashMap::from([
            (root, vec![]),
            (left, vec![root]),
            (left_child, vec![left]),
            (right, vec![root]),
        ]);
        let fake = FakeDerivation::default();

        // When they are derived with room to derive them all at once.
        derive_parents_first(&commits, REPO, &NoReplicationLag, |cs_id| {
            fake.derive(cs_id)
        })
        .await?;

        // Then each commit was derived once, and started only after its parents
        // finished.
        let steps = fake.steps.into_inner();
        assert_eq!(
            steps.len(),
            2 * commits.len(),
            "each commit should start and finish once"
        );
        let position = |step: Step| steps.iter().position(|s| *s == step).unwrap();
        for (child, parents) in &commits {
            for parent in parents {
                assert!(
                    position(Step::Finished(*parent)) < position(Step::Started(*child)),
                    "{child} started before its parent {parent} finished",
                );
            }
        }
        Ok(())
    }

    #[mononoke::test]
    async fn derives_no_more_commits_at_once_than_the_limit() -> Result<()> {
        // Given ten commits that do not depend on each other.
        let commits: HashMap<_, _> = (1..=10).map(|byte| (commit_id(byte), vec![])).collect();
        let fake = FakeDerivation::default();

        // When they are derived three at a time.
        set_switches(3, false);
        let results = derive_parents_first(&commits, REPO, &NoReplicationLag, |cs_id| {
            fake.derive(cs_id)
        })
        .await?;

        // Then all ten were derived, three at a time and never more.
        assert_eq!(results.len(), 10, "every commit should be derived");
        assert_eq!(
            most_running(&fake.steps.borrow()),
            3,
            "three derivations should run at once, and no more",
        );
        Ok(())
    }

    #[mononoke::test]
    async fn derives_a_commit_that_lists_a_parent_twice_once() -> Result<()> {
        set_switches(CONCURRENCY, false);
        // Given a commit whose parent appears twice in its list of parents.
        let (parent, child) = (commit_id(1), commit_id(2));
        let commits = HashMap::from([(parent, vec![]), (child, vec![parent, parent])]);
        let fake = FakeDerivation::default();

        // When they are derived.
        derive_parents_first(&commits, REPO, &NoReplicationLag, |cs_id| {
            fake.derive(cs_id)
        })
        .await?;

        // Then the parent and then the child were each derived once.
        assert_eq!(
            fake.steps.into_inner(),
            vec![
                Step::Started(parent),
                Step::Finished(parent),
                Step::Started(child),
                Step::Finished(child),
            ],
        );
        Ok(())
    }

    #[mononoke::test]
    async fn fails_when_parents_form_a_cycle() -> Result<()> {
        set_switches(CONCURRENCY, false);
        // Given two commits that each list the other as their parent, which a
        // real commit graph cannot contain.
        let (first, second) = (commit_id(1), commit_id(2));
        let commits = HashMap::from([(first, vec![second]), (second, vec![first])]);
        let fake = FakeDerivation::default();

        // When they are derived.
        let result = derive_parents_first(&commits, REPO, &NoReplicationLag, |cs_id| {
            fake.derive(cs_id)
        })
        .await;

        // Then neither was derived, and the call fails rather than reporting
        // success for commits it did not derive.
        assert!(result.is_err(), "a cycle should be reported as an error");
        assert_eq!(fake.steps.into_inner(), vec![], "nothing should be derived");
        Ok(())
    }

    #[mononoke::test]
    async fn records_the_descendants_of_a_failed_commit_without_deriving_them() -> Result<()> {
        set_switches(CONCURRENCY, false);
        // Given a commit that fails, its child and grandchild, and a commit
        // that does not depend on it.
        let (broken, child, grandchild, unrelated) =
            (commit_id(1), commit_id(2), commit_id(3), commit_id(4));
        let commits = HashMap::from([
            (broken, vec![]),
            (child, vec![broken]),
            (grandchild, vec![child]),
            (unrelated, vec![]),
        ]);
        let fake = FakeDerivation {
            failing: vec![broken],
            ..Default::default()
        };

        // When they are derived.
        let results = derive_parents_first(&commits, REPO, &NoReplicationLag, |cs_id| {
            fake.derive(cs_id)
        })
        .await?;

        // Then the failed commit keeps its error, its descendants are recorded
        // as failed through their parents without being tried, and the other
        // commit is derived.
        assert_matches!(results[&broken], Err(Failure::Error(_)));
        assert_matches!(
            results[&child],
            Err(Failure::ParentFailed { parent }) if parent == broken
        );
        assert_matches!(
            results[&grandchild],
            Err(Failure::ParentFailed { parent }) if parent == child
        );
        assert_matches!(results[&unrelated], Ok(1));
        let steps = fake.steps.into_inner();
        assert!(
            !steps.contains(&Step::Started(child)) && !steps.contains(&Step::Started(grandchild)),
            "the descendants of the failed commit should not be tried",
        );
        Ok(())
    }

    #[mononoke::test]
    async fn retries_a_failed_attempt_after_a_growing_delay() -> Result<()> {
        // Given an attempt that fails twice and then succeeds.
        tokio::time::pause();
        let attempts = AtomicUsize::new(0);
        let start = Instant::now();

        // When it is run with retries.
        let result = with_retries(RETRIES, commit_id(1), || async {
            if attempts.fetch_add(1, Ordering::Relaxed) < 2 {
                Err(anyhow::anyhow!("not yet").into())
            } else {
                Ok(5)
            }
        })
        .await;

        // Then the third attempt succeeds, after waiting one second and then
        // two.
        assert_matches!(result, Ok(5));
        assert_eq!(attempts.into_inner(), 3, "it should take three attempts");
        // Paused time rounds each sleep up to the next millisecond.
        assert_eq!(
            start.elapsed().as_secs(),
            3,
            "the retries should wait one second and then two",
        );
        Ok(())
    }

    #[mononoke::test]
    async fn gives_up_after_the_last_attempt() -> Result<()> {
        // Given an attempt that always fails.
        tokio::time::pause();
        let attempts = AtomicUsize::new(0);

        // When it is run with retries.
        let result = with_retries(RETRIES, commit_id(1), || async {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            Err(anyhow::anyhow!("attempt {attempt}").into())
        })
        .await;

        // Then it fails with the last attempt's error.
        assert_matches!(
            result,
            Err(Failure::Error(error)) if error.to_string() == "attempt 4"
        );
        assert_eq!(attempts.into_inner(), 4, "every attempt should be used");
        Ok(())
    }

    #[mononoke::test]
    async fn does_not_retry_a_commit_whose_derivation_is_blocked() -> Result<()> {
        // Given an attempt that finds derivation blocked in the repo config.
        tokio::time::pause();
        let attempts = AtomicUsize::new(0);
        let start = Instant::now();

        // When it is run with retries.
        let result = with_retries(RETRIES, commit_id(1), || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(Failure::Blocked {
                derived_data_type: "acl_manifests",
                commit: commit_id(2),
            })
        })
        .await;

        // Then it is tried once, without waiting, because the config would
        // block it again.
        assert_matches!(result, Err(Failure::Blocked { .. }));
        assert_eq!(attempts.into_inner(), 1, "it should not be retried");
        assert_eq!(start.elapsed(), Duration::ZERO, "it should not wait");
        Ok(())
    }

    /// `count` commits that do not depend on each other.
    fn independent_commits(count: u8) -> HashMap<ChangesetId, Vec<ChangesetId>> {
        (1..=count).map(|byte| (commit_id(byte), vec![])).collect()
    }

    #[mononoke::test]
    async fn a_lower_concurrency_takes_effect_while_deriving() -> Result<()> {
        // Given ten commits that do not depend on each other, derived four at
        // a time, and a concurrency switch lowered to one when the first
        // derivation finishes.
        set_switches(4, false);
        let commits = independent_commits(10);
        let fake = FakeDerivation::default();
        let finished = Cell::new(0);
        let derive = |cs_id| {
            let (fake, finished) = (&fake, &finished);
            async move {
                let result = fake.derive(cs_id).await;
                finished.set(finished.get() + 1);
                if finished.get() == 1 {
                    set_switches(1, false);
                    fake.note("lowered");
                }
                result
            }
        };

        // When they are derived.
        let results = derive_parents_first(&commits, REPO, &NoReplicationLag, derive).await?;

        // Then four ran at once until the switch was lowered, and after that
        // a derivation started only once none was running.
        assert_eq!(results.len(), 10, "every commit should be derived");
        let steps = fake.steps.into_inner();
        let lowered = steps
            .iter()
            .position(|step| *step == Step::Note("lowered"))
            .expect("the switch should have been lowered");
        assert_eq!(
            most_running(&steps[..lowered]),
            4,
            "four should run at once before the switch is lowered"
        );
        for (index, step) in steps.iter().enumerate().skip(lowered) {
            if matches!(step, Step::Started(_)) {
                assert_eq!(
                    running_after(&steps[..index]),
                    0,
                    "after the switch is lowered, a derivation should start only when none is running"
                );
            }
        }
        Ok(())
    }

    #[mononoke::test]
    async fn a_concurrency_below_one_counts_as_one() -> Result<()> {
        // Given three commits that do not depend on each other, and a
        // concurrency switch of zero.
        set_switches(0, false);
        let commits = independent_commits(3);
        let fake = FakeDerivation::default();

        // When they are derived.
        let results = derive_parents_first(&commits, REPO, &NoReplicationLag, |cs_id| {
            fake.derive(cs_id)
        })
        .await?;

        // Then all three were derived, one at a time.
        assert_eq!(results.len(), 3, "every commit should be derived");
        assert_eq!(most_running(&fake.steps.into_inner()), 1);
        Ok(())
    }

    #[mononoke::test]
    async fn pausing_stops_new_derivations_until_resumed() -> Result<()> {
        // Given six commits that do not depend on each other, derived two at a
        // time, a pause switch turned on when the first derivation finishes,
        // and turned off a minute later.
        tokio::time::pause();
        set_switches(2, false);
        let commits = independent_commits(6);
        let fake = FakeDerivation::default();
        let finished = Cell::new(0);
        let derive = |cs_id| {
            let (fake, finished) = (&fake, &finished);
            async move {
                let result = fake.derive(cs_id).await;
                finished.set(finished.get() + 1);
                if finished.get() == 1 {
                    set_switches(2, true);
                    fake.note("paused");
                }
                result
            }
        };
        let resume = async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            fake.note("resumed");
            set_switches(2, false);
        };

        // When they are derived.
        let (results, ()) = tokio::join!(
            derive_parents_first(&commits, REPO, &NoReplicationLag, derive),
            resume
        );

        // Then while paused, the derivation already running finished and none
        // started, and after resuming the rest were derived.
        assert_eq!(results?.len(), 6, "every commit should be derived");
        let steps = fake.steps.into_inner();
        let position = |note| steps.iter().position(|step| *step == Step::Note(note));
        let paused = position("paused").expect("the backfill should have been paused");
        let resumed = position("resumed").expect("the backfill should have been resumed");
        assert!(paused < resumed, "the pause should come before resuming");
        let while_paused = &steps[paused..resumed];
        assert!(
            !while_paused
                .iter()
                .any(|step| matches!(step, Step::Started(_))),
            "no derivation should start while paused: {while_paused:?}"
        );
        assert_eq!(
            running_after(&steps[..resumed]),
            0,
            "the derivation running when paused should finish"
        );
        Ok(())
    }

    #[mononoke::test]
    async fn waits_for_replication_before_starting_a_derivation() -> Result<()> {
        // Given three commits, and storage that lags for thirty seconds.
        tokio::time::pause();
        set_switches(CONCURRENCY, false);
        let commits = independent_commits(3);
        let fake = FakeDerivation::default();
        let lag = FakeLag::default();
        lag.lagging.store(true, Ordering::Relaxed);
        let catch_up = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let started = fake.steps.borrow().len();
            lag.lagging.store(false, Ordering::Relaxed);
            started
        };

        // When they are derived.
        let (results, started_while_lagging) = tokio::join!(
            derive_parents_first(&commits, REPO, &lag, |cs_id| fake.derive(cs_id)),
            catch_up
        );

        // Then none started while the storage lagged, and all three started
        // together after one check of the lag.
        assert_eq!(
            started_while_lagging, 0,
            "nothing should start while lagging"
        );
        assert_eq!(results?.len(), 3, "every commit should be derived");
        assert_eq!(
            lag.calls.into_inner(),
            1,
            "one check should cover all three"
        );
        Ok(())
    }

    #[mononoke::test]
    async fn a_pause_during_a_lag_wait_holds_back_derivations() -> Result<()> {
        // Given three commits, storage that lags for thirty seconds, and a
        // pause switch turned on during the lag and off thirty seconds after
        // the lag ends.
        tokio::time::pause();
        set_switches(CONCURRENCY, false);
        let commits = independent_commits(3);
        let fake = FakeDerivation::default();
        let lag = FakeLag::default();
        lag.lagging.store(true, Ordering::Relaxed);
        let pause_then_resume = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            set_switches(CONCURRENCY, true);
            lag.lagging.store(false, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(30)).await;
            let started = fake.steps.borrow().len();
            set_switches(CONCURRENCY, false);
            started
        };

        // When they are derived.
        let (results, started_while_paused) = tokio::join!(
            derive_parents_first(&commits, REPO, &lag, |cs_id| fake.derive(cs_id)),
            pause_then_resume
        );

        // Then none started once the lag ended while the repo was paused, and
        // all were derived once resumed.
        assert_eq!(started_while_paused, 0, "nothing should start while paused");
        assert_eq!(results?.len(), 3, "every commit should be derived");
        Ok(())
    }

    #[mononoke::test]
    async fn derivations_go_on_during_a_lag_wait_and_a_pause_during_it_holds_back_the_rest()
    -> Result<()> {
        // Given a commit whose derivation takes ten seconds, and a fast commit
        // with a child, derived two at a time; storage that starts to lag at
        // the second check of the lag and lags for thirty seconds; and a pause
        // switch turned on during the lag and off thirty seconds after it
        // ends.
        tokio::time::pause();
        set_switches(2, false);
        let (slow, fast, last) = (commit_id(1), commit_id(2), commit_id(3));
        let commits = HashMap::from([(slow, vec![]), (fast, vec![]), (last, vec![fast])]);
        let fake = FakeDerivation {
            slow: vec![slow],
            ..Default::default()
        };
        let lag = FakeLag {
            lags_from: 1,
            ..Default::default()
        };
        lag.lagging.store(true, Ordering::Relaxed);
        let pause_then_resume = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            set_switches(2, true);
            fake.note("paused");
            lag.lagging.store(false, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(30)).await;
            fake.note("resumed");
            set_switches(2, false);
        };

        // When they are derived.
        let (results, ()) = tokio::join!(
            derive_parents_first(&commits, REPO, &lag, |cs_id| fake.derive(cs_id)),
            pause_then_resume
        );

        // Then the slow derivation finished during the lag wait that began
        // when the fast one finished, and the last commit, held back by that
        // wait, did not start while paused.
        assert_eq!(results?.len(), 3, "every commit should be derived");
        let steps = fake.steps.into_inner();
        let position = |step| steps.iter().position(|s| *s == step);
        let paused = position(Step::Note("paused")).expect("the backfill should have been paused");
        let resumed =
            position(Step::Note("resumed")).expect("the backfill should have been resumed");
        assert!(
            position(Step::Finished(slow)) < Some(paused),
            "the slow derivation should finish during the lag wait: {steps:?}"
        );
        assert!(
            position(Step::Started(last)) > Some(resumed),
            "the last commit should start only after resuming: {steps:?}"
        );
        Ok(())
    }

    #[mononoke::test]
    async fn fails_when_replication_lag_cannot_be_checked() -> Result<()> {
        // Given a commit, and storage whose lag cannot be read.
        set_switches(CONCURRENCY, false);
        let commits = independent_commits(1);
        let fake = FakeDerivation::default();

        // When it is derived.
        let result =
            derive_parents_first(&commits, REPO, &BrokenLag, |cs_id| fake.derive(cs_id)).await;

        // Then the call fails without deriving it, rather than recording the
        // commit as failed.
        assert!(result.is_err(), "an unreadable lag should fail the call");
        assert_eq!(fake.steps.into_inner(), vec![], "nothing should be derived");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn every_derivation_shares_one_blobstore_throttle(fb: FacebookInit) -> Result<()> {
        // Given a backfill limited to 20 writes a second, which allows a burst
        // of 20 writes and then one every 50 milliseconds, and one without
        // limits.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let manager = repo.repo_derived_data().manager().clone();
        let limits = ThrottleOptions {
            write_qps: NonZeroU32::new(20),
            ..Default::default()
        };
        let throttled = RepoBackfill::new(manager.clone(), limits, Arc::new(NoReplicationLag));
        let unthrottled = RepoBackfill::new(
            manager,
            ThrottleOptions::default(),
            Arc::new(NoReplicationLag),
        );

        // When two copies of each backfill's manager, as two derivations hold
        // them, write 20 blobs each at the same time.
        let write_40 = |backfill: &RepoBackfill| {
            let managers = [backfill.manager.clone(), backfill.manager.clone()];
            let ctx = &ctx;
            async move {
                let start = Instant::now();
                futures::future::try_join_all(managers.into_iter().enumerate().map(
                    |(copy, manager)| async move {
                        for blob in 0..20 {
                            manager
                                .repo_blobstore()
                                .put(
                                    ctx,
                                    format!("test_blob_{copy}_{blob}"),
                                    BlobstoreBytes::from_bytes("blob"),
                                )
                                .await?;
                        }
                        anyhow::Ok(())
                    },
                ))
                .await?;
                anyhow::Ok(start.elapsed())
            }
        };
        let throttled_time = write_40(&throttled).await?;
        let unthrottled_time = write_40(&unthrottled).await?;

        // Then the throttled writes shared one limit, so the 20 writes past the
        // burst took about a second, while the others were not held back.
        assert!(
            throttled_time >= Duration::from_millis(900),
            "40 writes at 20 a second should take about a second, not {throttled_time:?}"
        );
        assert!(
            unthrottled_time < Duration::from_millis(900),
            "writes without limits should not be held back, but took {unthrottled_time:?}"
        );
        Ok(())
    }
}
