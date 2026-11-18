/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Which access label each read of an Hg manifest carries, as the storage
//! beneath the repo blobstore sees it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use acl_manifest::RootAclManifestId;
use anyhow::Result;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::Loadable;
use context::CoreContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStagePayload;
use derived_data_manager::PipelineDerivable;
use derived_data_manager::StageId;
use derived_data_manager::StageInputs;
use fbinit::FacebookInit;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use memblob::Memblob;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::access_labels;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use samplingblob::SamplingBlobstore;
use samplingblob::SamplingHandler;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;

use crate::Repo;

/// Records the access label of every read of an Hg manifest blob.
#[derive(Debug, Default)]
struct HgManifestReadLabels(Mutex<Vec<Option<&'static str>>>);

impl HgManifestReadLabels {
    /// The labels of the Hg manifest reads since the last call.
    fn take(&self) -> Vec<Option<&'static str>> {
        std::mem::take(&mut *self.0.lock().expect("poisoned lock"))
    }

    fn record(&self, ctx: &CoreContext, key: &str) {
        if key.contains("hgmanifest.sha1.") {
            self.0
                .lock()
                .expect("poisoned lock")
                .push(ctx.access_label());
        }
    }
}

impl SamplingHandler for HgManifestReadLabels {
    fn sample_get(
        &self,
        ctx: &CoreContext,
        key: &str,
        _value: Option<&BlobstoreGetData>,
    ) -> Result<()> {
        self.record(ctx, key);
        Ok(())
    }

    fn sample_is_present(
        &self,
        ctx: &CoreContext,
        key: &str,
        _value: &BlobstoreIsPresent,
    ) -> Result<()> {
        self.record(ctx, key);
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

async fn repo_recording_labels(fb: FacebookInit) -> Result<(Repo, Arc<HgManifestReadLabels>)> {
    let labels = Arc::new(HgManifestReadLabels::default());
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
        "the operation should read at least one Hg manifest",
    );
    assert!(
        labels.iter().all(|label| *label == Some(expected)),
        "every Hg manifest read should carry {expected:?}, got {labels:?}",
    );
}

#[mononoke::fbinit_test]
async fn test_hg_derivation_reads_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a repo that reads augmented manifests, with the root's Hg
    // changeset derived.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, child) = root_and_child(&ctx, &repo).await?;
    repo.derive_hg_changeset(&ctx, root).await?;
    labels.take();

    // When: deriving the child's Hg changeset from an unlabelled context.
    repo.derive_hg_changeset(&ctx, child).await?;

    // Then: every Hg manifest read made by the derivation is labelled as Hg
    // changeset derivation.
    assert_all_labelled(&labels.take(), access_labels::HG_CHANGESET_DERIVATION);
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
    // predecessors, are derived.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, _) = root_and_child(&ctx, &repo).await?;
    let manager = repo.repo_derived_data().manager();
    fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, vec![root]).await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root], None)
        .await?;
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
async fn test_hg_pipeline_stage_reads_are_labelled(fb: FacebookInit) -> Result<()> {
    // Given: a commit whose Hg changeset is derived, and a new child.
    set_read_from_augmented();
    let ctx = CoreContext::test_mock(fb);
    let (repo, labels) = repo_recording_labels(fb).await?;
    let (root, child) = root_and_child(&ctx, &repo).await?;
    repo.derive_hg_changeset(&ctx, root).await?;
    let manager = repo.repo_derived_data().manager();
    labels.take();

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

    // Then: every Hg manifest read of both stages is labelled as the Hg
    // pipeline.
    assert_all_labelled(&root_stage_labels, access_labels::HG_CHANGESET_PIPELINE);
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
