/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::num::NonZeroUsize;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use context::CoreContext;
use derived_data_manager::DerivedDataManager;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::stream::FuturesUnordered;
use mercurial_derivation::RootHgAugmentedManifestId;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use tokio_util::task::AbortOnDropHandle;
use topo_sort::TopoSortedDagTraversal;

/// What deriving a list of commits did with one of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitOutcome {
    /// The commit already had an augmented manifest.
    AlreadyDerived,
    /// The commit was derived. `count` is the number of commits this
    /// derivation derived: the commit itself and any of its ancestors that
    /// lacked augmented manifests. It is zero if another derivation got there
    /// first.
    Derived { count: u64 },
}

/// Derives augmented manifests, and the ACL manifests they depend on, for the
/// commits of one repo. Derivation is local: it never goes through the derived
/// data service.
pub struct RepoBackfill {
    manager: DerivedDataManager,
    concurrency: NonZeroUsize,
}

impl RepoBackfill {
    pub fn new(manager: DerivedDataManager, concurrency: NonZeroUsize) -> Self {
        Self {
            manager,
            concurrency,
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
    pub async fn derive_commits(
        &self,
        ctx: &CoreContext,
        mut commits: HashMap<ChangesetId, Vec<ChangesetId>>,
    ) -> Result<HashMap<ChangesetId, CommitOutcome>> {
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

        let counts = derive_parents_first(&commits, self.concurrency, |cs_id| {
            self.derive_commit(ctx, cs_id)
        })
        .await?;

        Ok(derived
            .into_keys()
            .map(|cs_id| (cs_id, CommitOutcome::AlreadyDerived))
            .chain(
                counts
                    .into_iter()
                    .map(|(cs_id, count)| (cs_id, CommitOutcome::Derived { count })),
            )
            .collect())
    }

    /// Derives `cs_id` on a task of its own. Deriving a commit that has an Hg
    /// changeset does its CPU work on the task that polls it, so without this,
    /// derivations running at once would share one thread.
    ///
    /// Where ancestors were derived out of order, `derive_heads` can derive an
    /// ancestor that is already derived. The default blobstore put behaviour,
    /// which does not overwrite existing keys, keeps what is stored.
    async fn derive_commit(&self, ctx: &CoreContext, cs_id: ChangesetId) -> Result<u64> {
        let manager = self.manager.clone();
        let ctx = ctx.clone();
        let derived = AbortOnDropHandle::new(mononoke::spawn_task(async move {
            manager
                .derive_heads::<RootHgAugmentedManifestId>(ctx, vec![cs_id], None, None)
                .await
        }))
        .await
        .with_context(|| format!("The task deriving {cs_id} panicked"))?;
        derived.with_context(|| format!("Failed to derive the augmented manifest of {cs_id}"))
    }
}

/// Derives each commit of `commits`, which maps each commit to its parents,
/// with `derive`, running up to `concurrency` derivations at once, and returns
/// what each derivation returned. A commit starts only once its parents in
/// `commits` have been derived. Parents outside `commits` are ignored.
async fn derive_parents_first<F, Fut>(
    commits: &HashMap<ChangesetId, Vec<ChangesetId>>,
    concurrency: NonZeroUsize,
    derive: F,
) -> Result<HashMap<ChangesetId, u64>>
where
    F: Fn(ChangesetId) -> Fut,
    Fut: Future<Output = Result<u64>>,
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

    let mut counts = HashMap::with_capacity(commits.len());
    let mut running = FuturesUnordered::new();
    loop {
        let free = concurrency.get() - running.len();
        running.extend(
            dag.drain(free)
                .map(|cs_id| derive(cs_id).map_ok(move |count| (cs_id, count))),
        );
        let Some((cs_id, count)) = running.try_next().await? else {
            break;
        };
        counts.insert(cs_id, count);
        dag.visited(cs_id);
    }

    ensure!(
        counts.len() == commits.len(),
        "{} of {} commits were never derived, because their parents form a cycle",
        commits.len() - counts.len(),
        commits.len(),
    );
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;

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
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let base = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file", "base")
            .commit()
            .await?;
        repo.repo_derived_data()
            .manager()
            .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![base], None, None)
            .await?;
        Ok((ctx, repo, base))
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

    fn backfill(repo: &TestRepo) -> RepoBackfill {
        RepoBackfill::new(repo.repo_derived_data().manager().clone(), CONCURRENCY)
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
        assert_eq!(
            outcomes,
            HashMap::from([
                (bottom, CommitOutcome::Derived { count: 1 }),
                (top, CommitOutcome::Derived { count: 1 }),
            ]),
        );
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
        assert_eq!(
            outcomes,
            HashMap::from([
                (base, CommitOutcome::AlreadyDerived),
                (underived, CommitOutcome::Derived { count: 1 }),
            ]),
        );
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
        assert_eq!(
            outcomes,
            HashMap::from([(top, CommitOutcome::Derived { count: 2 })]),
        );
        assert!(
            has_augmented_manifest(&ctx, &repo, bottom).await?,
            "the bottom of the stack should have an augmented manifest",
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
    /// overlap.
    #[derive(Default)]
    struct FakeDerivation {
        steps: RefCell<Vec<Step>>,
    }

    impl FakeDerivation {
        async fn derive(&self, cs_id: ChangesetId) -> Result<u64> {
            self.steps.borrow_mut().push(Step::Started(cs_id));
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            self.steps.borrow_mut().push(Step::Finished(cs_id));
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

    fn commit_id(byte: u8) -> ChangesetId {
        ChangesetId::from_bytes([byte; 32]).expect("32 bytes should make a changeset id")
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
        let counts = derive_parents_first(&commits, NonZeroUsize::new(3).unwrap(), |cs_id| {
            fake.derive(cs_id)
        })
        .await?;

        // Then all ten were derived, three at a time and never more.
        assert_eq!(counts.len(), 10, "every commit should be derived");
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
}
