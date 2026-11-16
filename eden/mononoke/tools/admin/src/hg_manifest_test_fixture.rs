/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! A small repo for testing commands that read Hg manifests from augmented
//! manifests.

use std::sync::Arc;

use anyhow::Result;
use blobstore::Loadable;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::hashmap;
use memblob::Memblob;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_types::HgManifestId;
use mononoke_types::ChangesetId;
use mononoke_types::FileType;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use test_repo_factory::TestRepoFactory;
use tests_utils::BasicTestRepo;
use tests_utils::CreateCommitContext;
use tests_utils::hg_manifest_reads::HgManifestReadBlocker;

/// Hg changesets are derived without their augmented manifests, so that
/// deriving one never reads Hg manifests.
pub(crate) fn read_from_augmented(on: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(on),
        "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
    })
}

/// A root commit with files `a` and `dir/b`, and a child that removes `a`,
/// changes `dir/b` and adds `dir/c` and `e` (executable). Both have hg
/// changesets; only the root has an augmented manifest. Hg manifest reads
/// fail once `blocker` is armed.
pub(crate) struct HgManifestFixture {
    factory: TestRepoFactory,
    repo: BasicTestRepo,
    pub(crate) blocker: Arc<HgManifestReadBlocker>,
    pub(crate) root: ChangesetId,
    pub(crate) child: ChangesetId,
    pub(crate) child_mf: HgManifestId,
}

impl HgManifestFixture {
    pub(crate) async fn new(ctx: &CoreContext) -> Result<Self> {
        let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
        let mut factory = TestRepoFactory::new(ctx.fb)?;
        factory.with_blobstore(blobstore);
        let repo: BasicTestRepo = factory.build().await?;
        let root = CreateCommitContext::new_root(ctx, &repo)
            .add_file("a", "a\n")
            .add_file("dir/b", "b\n")
            .commit()
            .await?;
        let child = CreateCommitContext::new(ctx, &repo, vec![root])
            .delete_file("a")
            .add_file("dir/b", "b2\n")
            .add_file("dir/c", "c\n")
            .add_file_with_type("e", "e\n", FileType::Executable)
            .commit()
            .await?;
        let child_hg = with_just_knobs_async(
            read_from_augmented(false),
            async {
                repo.derive_hg_changeset(ctx, root).await?;
                // Derivation would derive the child's augmented manifest along
                // with its Hg changeset, so store it as an upload does.
                fixtures::store_hg_changesets_as_uploaded(ctx, &repo, vec![child]).await?;
                repo.derive_hg_changeset(ctx, child).await
            }
            .boxed(),
        )
        .await?;
        let child_mf = child_hg
            .load(ctx, repo.repo_blobstore())
            .await?
            .manifestid();
        let fixture = Self {
            factory,
            repo,
            blocker,
            root,
            child,
            child_mf,
        };
        fixture.derive_augmented(ctx, root).await?;
        Ok(fixture)
    }

    /// The fixture's repo as the facet container a command uses.
    pub(crate) async fn repo<R>(&self) -> Result<R>
    where
        R: for<'builder> facet::AsyncBuildable<
                'builder,
                test_repo_factory::TestRepoFactoryBuilder<'builder>,
            >,
    {
        Ok(self.factory.build().await?)
    }

    pub(crate) async fn derive_augmented(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<()> {
        self.repo
            .repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
        Ok(())
    }
}
