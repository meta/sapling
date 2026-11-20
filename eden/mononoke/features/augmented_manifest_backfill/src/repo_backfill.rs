/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::error::Error as StdError;
use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
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
use thiserror::Error;
use tokio_util::task::AbortOnDropHandle;
use topo_sort::TopoSortedDagTraversal;
use tracing::warn;

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

/// Derives augmented manifests, and the ACL manifests they depend on, for the
/// commits of one repo. Derivation is local: it never goes through the derived
/// data service.
pub struct RepoBackfill {
    manager: DerivedDataManager,
    concurrency: NonZeroUsize,
    retries: Retries,
}

impl RepoBackfill {
    pub fn new(manager: DerivedDataManager, concurrency: NonZeroUsize) -> Self {
        Self {
            manager,
            concurrency,
            retries: RETRIES,
        }
    }

    /// Derives the commits of `commits`, which maps each commit to its parents,
    /// and returns what was done with each one.
    ///
    /// Commits that already have augmented manifests are skipped. Up to
    /// `concurrency` commits are derived at once, and a commit waits only for
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
    /// manifests, if it cannot check which commits are derived, or if the
    /// parents form a cycle.
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

        let results = derive_parents_first(&commits, self.concurrency, |cs_id| {
            with_retries(self.retries, cs_id, move || {
                self.try_derive_commit(ctx, cs_id)
            })
        })
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
/// with `derive`, running up to `concurrency` derivations at once, and returns
/// what each derivation returned. A commit starts only once its parents in
/// `commits` have finished. A commit whose parent in `commits` failed is
/// recorded as failed without calling `derive`. Parents outside `commits` are
/// ignored.
async fn derive_parents_first<F, Fut>(
    commits: &HashMap<ChangesetId, Vec<ChangesetId>>,
    concurrency: NonZeroUsize,
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
        while running.len() < concurrency.get() {
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
        let Some((cs_id, result)) = running.next().await else {
            break;
        };
        results.insert(cs_id, result);
        dag.visited(cs_id);
    }

    ensure!(
        results.len() == commits.len(),
        "{} of {} commits were never derived, because their parents form a cycle",
        commits.len() - results.len(),
        commits.len(),
    );
    Ok(results)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use assert_matches::assert_matches;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bonsai_hg_mapping::BonsaiHgMappingEntry;
    use bonsai_hg_mapping::BonsaiHgMappingRef;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use mercurial_types::HgChangesetId;
    use mononoke_types::DerivableType;
    use repo_blobstore::RepoBlobstore;
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
    const CONCURRENCY: NonZeroUsize = NonZeroUsize::new(10).unwrap();

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
            ..RepoBackfill::new(repo.repo_derived_data().manager().clone(), CONCURRENCY)
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
    }

    /// Stands in for derivation. It records when each commit starts and
    /// finishes, and yields in between, so that derivations running at once
    /// overlap. Commits in `failing` fail.
    #[derive(Default)]
    struct FakeDerivation {
        steps: RefCell<Vec<Step>>,
        failing: Vec<ChangesetId>,
    }

    impl FakeDerivation {
        async fn derive(&self, cs_id: ChangesetId) -> Result<u64, Failure> {
            self.steps.borrow_mut().push(Step::Started(cs_id));
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            self.steps.borrow_mut().push(Step::Finished(cs_id));
            if self.failing.contains(&cs_id) {
                return Err(anyhow::anyhow!("{cs_id} is broken").into());
            }
            Ok(1)
        }

        /// The most derivations that were running at the same time.
        fn most_running(&self) -> usize {
            self.steps
                .borrow()
                .iter()
                .scan(0, |running, step| {
                    *running = match step {
                        Step::Started(_) => *running + 1,
                        Step::Finished(_) => *running - 1,
                    };
                    Some(*running)
                })
                .max()
                .unwrap_or(0)
        }
    }

    #[mononoke::test]
    async fn starts_a_commit_only_after_its_parents_finish() -> Result<()> {
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
        derive_parents_first(&commits, CONCURRENCY, |cs_id| fake.derive(cs_id)).await?;

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
        let results = derive_parents_first(&commits, NonZeroUsize::new(3).unwrap(), |cs_id| {
            fake.derive(cs_id)
        })
        .await?;

        // Then all ten were derived, three at a time and never more.
        assert_eq!(results.len(), 10, "every commit should be derived");
        assert_eq!(
            fake.most_running(),
            3,
            "three derivations should run at once, and no more",
        );
        Ok(())
    }

    #[mononoke::test]
    async fn derives_a_commit_that_lists_a_parent_twice_once() -> Result<()> {
        // Given a commit whose parent appears twice in its list of parents.
        let (parent, child) = (commit_id(1), commit_id(2));
        let commits = HashMap::from([(parent, vec![]), (child, vec![parent, parent])]);
        let fake = FakeDerivation::default();

        // When they are derived.
        derive_parents_first(&commits, CONCURRENCY, |cs_id| fake.derive(cs_id)).await?;

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
        // Given two commits that each list the other as their parent, which a
        // real commit graph cannot contain.
        let (first, second) = (commit_id(1), commit_id(2));
        let commits = HashMap::from([(first, vec![second]), (second, vec![first])]);
        let fake = FakeDerivation::default();

        // When they are derived.
        let result = derive_parents_first(&commits, CONCURRENCY, |cs_id| fake.derive(cs_id)).await;

        // Then neither was derived, and the call fails rather than reporting
        // success for commits it did not derive.
        assert!(result.is_err(), "a cycle should be reported as an error");
        assert_eq!(fake.steps.into_inner(), vec![], "nothing should be derived");
        Ok(())
    }

    #[mononoke::test]
    async fn records_the_descendants_of_a_failed_commit_without_deriving_them() -> Result<()> {
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
        let results =
            derive_parents_first(&commits, CONCURRENCY, |cs_id| fake.derive(cs_id)).await?;

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
}
