/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Which access label each read and write of an Hg manifest carries, as the
//! storage beneath the repo blobstore sees it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use acl_manifest::RootAclManifestId;
use anyhow::Result;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStagePayload;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageInputs;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use memblob::Memblob;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::access_labels;
use mercurial_derivation::rebuild_hg_manifests::HgTreeRebuilder;
use mercurial_derivation::rebuild_hg_manifests::RebuildAction;
use mercurial_derivation::rebuild_hg_manifests::TreeScope;
use mercurial_derivation::rebuild_hg_manifests::commit_hg_roots;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths::RestrictedPathsRef;
use samplingblob::SamplingBlobstore;
use samplingblob::SamplingHandler;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;

use crate::Repo;

/// Records the access label of every read and every write of an Hg manifest
/// blob.
#[derive(Debug, Default)]
struct HgManifestLabels {
    reads: Mutex<Vec<Option<&'static str>>>,
    writes: Mutex<Vec<Option<&'static str>>>,
}

impl HgManifestLabels {
    /// The labels of the Hg manifest reads since the last call.
    fn take(&self) -> Vec<Option<&'static str>> {
        std::mem::take(&mut *self.reads.lock().expect("poisoned lock"))
    }

    /// The labels of the Hg manifest writes since the last call.
    fn take_writes(&self) -> Vec<Option<&'static str>> {
        std::mem::take(&mut *self.writes.lock().expect("poisoned lock"))
    }

    fn record(list: &Mutex<Vec<Option<&'static str>>>, ctx: &CoreContext, key: &str) {
        if key.contains("hgmanifest.sha1.") {
            list.lock().expect("poisoned lock").push(ctx.access_label());
        }
    }
}

impl SamplingHandler for HgManifestLabels {
    fn sample_get(
        &self,
        ctx: &CoreContext,
        key: &str,
        _value: Option<&BlobstoreGetData>,
    ) -> Result<()> {
        Self::record(&self.reads, ctx, key);
        Ok(())
    }

    fn sample_is_present(
        &self,
        ctx: &CoreContext,
        key: &str,
        _value: &BlobstoreIsPresent,
    ) -> Result<()> {
        Self::record(&self.reads, ctx, key);
        Ok(())
    }

    fn sample_put(&self, ctx: &CoreContext, key: &str, _value: &BlobstoreBytes) -> Result<()> {
        Self::record(&self.writes, ctx, key);
        Ok(())
    }
}

/// Turns `hgmanifest_read_from_augmented` on for every task on this thread.
fn set_read_from_augmented() {
    override_just_knobs(JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke:hgmanifest_read_from_augmented".to_string(),
            KnobVal::Bool(true),
        ),
        (
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string(),
            KnobVal::Bool(false),
        ),
    ])));
}

async fn repo_recording_labels(fb: FacebookInit) -> Result<(Repo, Arc<HgManifestLabels>)> {
    let labels = Arc::new(HgManifestLabels::default());
    let repo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(SamplingBlobstore::new(
            Memblob::default(),
            labels.clone(),
        )))
        .build()
        .await?;
    Ok((repo, labels))
}

/// A root commit and a child that changes a file in a directory, so deriving
/// the child reads its parent's Hg manifests.
async fn root_and_child(ctx: &CoreContext, repo: &Repo) -> Result<(ChangesetId, ChangesetId)> {
    let root = CreateCommitContext::new_root(ctx, repo)
        .add_file("a.txt", "a")
        .add_file("dir/b.txt", "b")
        .commit()
        .await?;
    let child = CreateCommitContext::new(ctx, repo, vec![root])
        .add_file("dir/b.txt", "b2")
        .add_file("dir/c.txt", "c")
        .commit()
        .await?;
    Ok((root, child))
}

fn assert_all_labelled(labels: &[Option<&'static str>], expected: &'static str) {
    assert!(
        !labels.is_empty(),
        "the operation should access at least one Hg manifest",
    );
    assert!(
        labels.iter().all(|label| *label == Some(expected)),
        "every Hg manifest access should carry {expected:?}, got {labels:?}",
    );
}

#[mononoke::fbinit_test]
async fn test_hg_derivation_reads_and_writes_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a repo that reads augmented manifests, with the root's Hg
    // changeset derived.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, child) = root_and_child(&ctx, &repo).await?;
    repo.derive_hg_changeset(&ctx, root).await?;
    labels.take();
    labels.take_writes();

    // When: deriving the child's Hg changeset through the derived data
    // manager, which holds writes in memory until it flushes them, from an
    // unlabelled context.
    repo.derive_hg_changeset(&ctx, child).await?;

    // Then: every Hg manifest read and write made by the derivation is
    // labelled as Hg changeset derivation.
    assert_all_labelled(&labels.take(), access_labels::HG_CHANGESET_DERIVATION);
    assert_all_labelled(
        &labels.take_writes(),
        access_labels::HG_CHANGESET_DERIVATION,
    );
    Ok(())
}

/// A root commit and a child, both with Hg changesets stored the way a client
/// upload stores them and their ACL manifests derived, so that deriving their
/// augmented manifests builds them from their Hg manifests.
async fn uploaded_root_and_child(
    ctx: &CoreContext,
    repo: &Repo,
) -> Result<(ChangesetId, ChangesetId)> {
    let (root, child) = root_and_child(ctx, repo).await?;
    fixtures::store_hg_changesets_as_uploaded(ctx, repo, vec![root, child]).await?;
    repo.repo_derived_data()
        .manager()
        .derive_exactly_batch::<RootAclManifestId>(ctx, vec![root, child], None)
        .await?;
    Ok((root, child))
}

/// A root commit with its augmented manifest derived, and a child, both with
/// uploaded Hg changesets. Returns the child and the root's augmented
/// manifest.
async fn uploaded_child_of_derived_root(
    ctx: &CoreContext,
    repo: &Repo,
) -> Result<(BonsaiChangeset, RootHgAugmentedManifestId)> {
    let (root, child) = uploaded_root_and_child(ctx, repo).await?;
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, vec![root], None)
        .await?;
    let root_aug = manager
        .fetch_derived::<RootHgAugmentedManifestId>(ctx, root, None)
        .await?
        .expect("the root's augmented manifest was just derived");
    let bonsai = child.load(ctx, repo.repo_blobstore()).await?;
    Ok((bonsai, root_aug))
}

#[mononoke::fbinit_test]
async fn test_augmented_derivation_from_hg_reads_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a root commit with its augmented manifest derived, and a child,
    // both with uploaded Hg changesets.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (bonsai, root_aug) = uploaded_child_of_derived_root(&ctx, &repo).await?;
    let manager = repo.repo_derived_data().manager();
    labels.take();

    // When: deriving the child's augmented manifest from an unlabelled
    // context.
    RootHgAugmentedManifestId::derive_single(
        &ctx,
        &manager.derivation_context(None),
        bonsai,
        vec![root_aug],
        None,
    )
    .await?;

    // Then: every Hg manifest read is labelled as augmented derivation from
    // Hg manifests.
    assert_all_labelled(&labels.take(), access_labels::AUGMENTED_DERIVATION_FROM_HG);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_building_from_hg_outside_derivation_is_unlabelled(fb: FacebookInit) -> Result<()> {
    // Given: a root commit with its augmented manifest derived, and a child,
    // both with uploaded Hg changesets.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (bonsai, root_aug) = uploaded_child_of_derived_root(&ctx, &repo).await?;
    let manager = repo.repo_derived_data().manager();
    labels.take();

    // When: building the child's augmented manifest from its Hg manifest
    // outside derivation, as `mononoke_admin derived-data verify-aug-direct`
    // does.
    RootHgAugmentedManifestId::derive_from_mapped_hg_with_parents(
        &ctx,
        &manager.derivation_context(None),
        &bonsai,
        vec![root_aug.hg_augmented_manifest_id()],
    )
    .await?
    .expect("the child has an Hg changeset");

    // Then: its Hg manifest reads carry no label, so the read check reports
    // them.
    let labels = labels.take();
    assert!(
        !labels.is_empty(),
        "building from Hg should read Hg manifests"
    );
    assert!(
        labels.iter().all(Option::is_none),
        "no read should be labelled, got {labels:?}",
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_augmented_derivation_from_hg_reads_are_labelled(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a root commit and a child with uploaded Hg changesets.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, child) = uploaded_root_and_child(&ctx, &repo).await?;
    labels.take();

    // When: deriving both augmented manifests in one batch from an
    // unlabelled context.
    repo.repo_derived_data()
        .manager()
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![root, child], None)
        .await?;

    // Then: every Hg manifest read is labelled as augmented derivation from
    // Hg manifests.
    assert_all_labelled(&labels.take(), access_labels::AUGMENTED_DERIVATION_FROM_HG);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_hg_manifest_load_is_unlabelled(fb: FacebookInit) -> Result<()> {
    // Given: a commit whose Hg changeset is derived.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, _) = root_and_child(&ctx, &repo).await?;
    let hg_cs_id = repo.derive_hg_changeset(&ctx, root).await?;
    let manifest_id = hg_cs_id
        .load(&ctx, repo.repo_blobstore())
        .await?
        .manifestid();
    labels.take();

    // When: loading its root Hg manifest directly.
    manifest_id.load(&ctx, repo.repo_blobstore()).await?;

    // Then: the read carries no label.
    assert_eq!(labels.take(), vec![None]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_augmented_backfill_reads_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a commit whose Hg changeset and ACL manifest, the backfill's
    // predecessors, are stored, and whose augmented manifest is not. Deriving
    // the Hg changeset through the manager would derive the augmented
    // manifest first, so the Hg changeset is stored like an upload.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, _) = uploaded_root_and_child(&ctx, &repo).await?;
    let manager = repo.repo_derived_data().manager();
    labels.take();

    // When: backfilling its augmented manifest from its Hg manifest.
    manager
        .unsafe_derive_untopologically::<RootHgAugmentedManifestId>(&ctx, root, None)
        .await?;

    // Then: every Hg manifest read is labelled as the augmented backfill.
    assert_all_labelled(&labels.take(), access_labels::AUGMENTED_BACKFILL_FROM_HG);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hg_pipeline_stage_reads_and_writes_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a commit whose Hg changeset is derived, and a new child.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, child) = root_and_child(&ctx, &repo).await?;
    repo.derive_hg_changeset(&ctx, root).await?;
    let manager = repo.repo_derived_data().manager();
    labels.take();
    labels.take_writes();

    // When: the pipeline derives the child's root manifest stage, seeding it
    // from the parent's derived changeset, and then its finalize stage.
    let root_stage = DerivationStagePayload::Manifest(ManifestStagePayload {
        path: MPath::ROOT,
        deps: vec![].into(),
    });
    let root_outputs = manager
        .derive_stage_batch::<MappedHgChangesetId>(&ctx, vec![child], &root_stage)
        .await?;
    let root_stage_labels = labels.take();
    let root_stage_writes = labels.take_writes();
    let manager = manager.with_stage_inputs(StageInputs::new(
        &root_outputs.outputs.into_iter().collect::<Vec<_>>(),
    )?);
    manager
        .derive_stage_batch::<MappedHgChangesetId>(
            &ctx,
            vec![child],
            &DerivationStagePayload::Finalize,
        )
        .await?;

    // Then: every Hg manifest read of both stages, and every Hg manifest the
    // root stage writes, is labelled as the Hg pipeline.
    assert_all_labelled(&root_stage_labels, access_labels::HG_CHANGESET_PIPELINE);
    assert_all_labelled(&root_stage_writes, access_labels::HG_CHANGESET_PIPELINE);
    assert_all_labelled(&labels.take(), access_labels::HG_CHANGESET_PIPELINE);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hg_pipeline_seeding_from_derived_reads_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a commit whose Hg changeset is derived.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, _) = root_and_child(&ctx, &repo).await?;
    repo.derive_hg_changeset(&ctx, root).await?;
    let manager = repo.repo_derived_data().manager();
    let mapped = manager
        .fetch_derived::<MappedHgChangesetId>(&ctx, root, None)
        .await?
        .expect("the root's Hg changeset was just derived");
    labels.take();

    // When: the pipeline seeds a directory stage from the derived changeset.
    MappedHgChangesetId::extract_stage_output_from_derived(
        &ctx,
        &manager.derivation_context(None),
        &mapped,
        &StageId::Manifest(MPath::new("dir")?),
    )
    .await?;

    // Then: every Hg manifest read is labelled as the Hg pipeline.
    assert_all_labelled(&labels.take(), access_labels::HG_CHANGESET_PIPELINE);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hg_pipeline_terminal_fetch_does_not_read_manifests(fb: FacebookInit) -> Result<()> {
    // Given: a commit whose Hg changeset is derived, and the pipeline's
    // finalize stage reading from the Bonsai-to-Hg mapping.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, _) = root_and_child(&ctx, &repo).await?;
    repo.derive_hg_changeset(&ctx, root).await?;
    let manager = repo.repo_derived_data().manager();
    labels.take();

    // When: the pipeline fetches the commit's finalize stage output.
    let outputs = MappedHgChangesetId::fetch_stage_outputs(
        &ctx,
        &manager.derivation_context(None),
        &StageId::Finalize,
        vec![root],
    )
    .await?;

    assert!(outputs.contains_key(&root));
    assert!(labels.take().is_empty());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_hg_manifest_rebuild_reads_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a commit with an Hg changeset and an augmented manifest.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, _) = root_and_child(&ctx, &repo).await?;
    repo.derive_hg_changeset(&ctx, root).await?;
    repo.repo_derived_data()
        .manager()
        .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![root], None, None)
        .await?;
    let roots =
        commit_hg_roots(&ctx, repo.repo_blobstore(), repo.bonsai_hg_mapping(), root).await?;
    labels.take();

    // When: comparing its rebuilt Hg trees with the stored ones.
    HgTreeRebuilder::new(
        repo.repo_blobstore().clone(),
        repo.restricted_paths().config_based().clone(),
    )
    .rebuild(&ctx, roots, TreeScope::Whole, RebuildAction::Compare)
    .try_collect::<Vec<_>>()
    .await?;

    // Then: every Hg manifest read is labelled as the rebuild.
    assert_all_labelled(&labels.take(), access_labels::HG_MANIFEST_REBUILD);
    Ok(())
}
