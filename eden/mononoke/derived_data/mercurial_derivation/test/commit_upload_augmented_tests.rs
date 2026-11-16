/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Commit upload with `scm/mononoke:hgmanifest_read_from_augmented` on reads
//! augmented manifests instead of Hg manifests.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use blobrepo_hg::ChangesetHandle;
use blobrepo_hg::CreateChangeset;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bytes::Bytes;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::StreamExt;
use futures::future;
use futures::stream;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use manifest::Entry;
use manifest::ManifestOps;
use memblob::Memblob;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::upload_augmented_manifest::build_augmented_manifests_for_uploaded_trees;
use mercurial_derivation::upload_augmented_manifest::store_uploaded_tree_envelopes;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::MPath;
use mercurial_types::NULL_HASH;
use mercurial_types::NonRootMPath;
use mercurial_types::RepoPath;
use mercurial_types::blobs::ChangesetMetadata;
use mercurial_types::blobs::UploadHgNodeHash;
use mercurial_types::blobs::UploadHgTreeEntry;
use mercurial_types::fetch_manifest_envelope;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use repo_blobstore::RepoBlobstoreArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths::RestrictedPathsArc;
use restricted_paths::RestrictedPathsRef;
use samplingblob::SamplingBlobstore;
use samplingblob::SamplingHandler;
use scuba_ext::MononokeScubaSampleBuilder;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;

use crate::Repo;
use crate::utils::upload_file_no_parents;

const READ_FROM_AUGMENTED: &str = "scm/mononoke:hgmanifest_read_from_augmented";
const DERIVE_AUGMENTED_WITH_HG_CHANGESET: &str =
    "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset";

/// Augmented manifests of existing commits are derived only on request.
fn read_from_augmented(on: bool) {
    override_just_knobs(JustKnobsInMemory::new(HashMap::from([
        (READ_FROM_AUGMENTED.to_string(), KnobVal::Bool(on)),
        (
            DERIVE_AUGMENTED_WITH_HG_CHANGESET.to_string(),
            KnobVal::Bool(false),
        ),
    ])));
}

/// Once blocked, fails and counts every read of an Hg manifest blob, standing
/// in for a store where they have been deleted.
#[derive(Debug, Default)]
struct BlockHgManifestReads {
    blocked: AtomicBool,
    reads: AtomicUsize,
}

impl BlockHgManifestReads {
    fn check(&self, key: &str) -> Result<()> {
        if self.blocked.load(Ordering::Relaxed) && key.contains(".hgmanifest.sha1.") {
            self.reads.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!("blocked read of Hg manifest {key}");
        }
        Ok(())
    }

    fn block(&self) {
        self.blocked.store(true, Ordering::Relaxed);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

impl SamplingHandler for BlockHgManifestReads {
    fn sample_get(
        &self,
        _ctx: &CoreContext,
        key: &str,
        _value: Option<&BlobstoreGetData>,
    ) -> Result<()> {
        self.check(key)
    }

    fn sample_is_present(
        &self,
        _ctx: &CoreContext,
        key: &str,
        _value: &BlobstoreIsPresent,
    ) -> Result<()> {
        self.check(key)
    }
}

async fn repo_with_blockable_hg_reads(
    fb: FacebookInit,
) -> Result<(Repo, Arc<BlockHgManifestReads>)> {
    let hg_reads = Arc::new(BlockHgManifestReads::default());
    let repo: Repo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(SamplingBlobstore::new(
            Memblob::default(),
            hg_reads.clone(),
        )))
        .build()
        .await?;
    Ok((repo, hg_reads))
}

/// An existing commit with files `a` and `dir/b`, with its Hg changeset and,
/// if `augmented`, its augmented manifests derived.
async fn existing_commit(
    ctx: &CoreContext,
    repo: &Repo,
    augmented: bool,
) -> Result<(HgChangesetId, HgManifestId)> {
    let cs_id = CreateCommitContext::new_root(ctx, repo)
        .add_file("a", "a")
        .add_file("dir/b", "b")
        .commit()
        .await?;
    if !augmented {
        fixtures::store_hg_changesets_as_uploaded(ctx, repo, vec![cs_id]).await?;
    }
    let hg_cs_id = repo.derive_hg_changeset(ctx, cs_id).await?;
    if augmented {
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
    }
    let hg_cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
    Ok((hg_cs_id, hg_cs.manifestid()))
}

async fn upload_file(
    ctx: &CoreContext,
    repo: &Repo,
    path: &str,
    contents: String,
) -> Result<HgFileNodeId> {
    let (id, upload) = upload_file_no_parents(ctx.clone(), repo, contents, &RepoPath::file(path)?);
    upload.await?;
    Ok(id)
}

/// Upload a root tree as tree upload does: its Hg manifest, then, if
/// `augmented`, its augmented manifest.
async fn upload_root_tree(
    ctx: &CoreContext,
    repo: &Repo,
    contents: String,
    parents: &[HgManifestId],
    augmented: bool,
) -> Result<HgManifestId> {
    let (id, upload) = UploadHgTreeEntry {
        upload_node_id: UploadHgNodeHash::Generate,
        contents: Bytes::from(contents),
        p1: parents.first().map(|id| id.into_nodehash()),
        p2: parents.get(1).map(|id| id.into_nodehash()),
        path: RepoPath::RootPath,
        computed_node_id: None,
    }
    .upload(ctx.clone(), repo.repo_blobstore_arc())?;
    upload.await?;
    if augmented {
        let envelope = fetch_manifest_envelope(ctx, repo.repo_blobstore(), id).await?;
        let blobstore: Arc<dyn KeyedBlobstore> = repo.repo_blobstore_arc();
        let built = build_augmented_manifests_for_uploaded_trees(
            ctx,
            &blobstore,
            repo.restricted_paths().config_based(),
            vec![envelope],
        )
        .await?;
        store_uploaded_tree_envelopes(ctx, &blobstore, &built).await?;
    }
    Ok(id)
}

/// Create a changeset from an uploaded root tree, as commit upload does.
fn create_changeset(
    ctx: &CoreContext,
    repo: &Repo,
    root: HgManifestId,
    files: &[&str],
    parents: Vec<ChangesetHandle>,
) -> Result<ChangesetHandle> {
    let mut parents = parents.into_iter();
    // Commit upload sends no root tree for a commit with no files.
    let root_manifest = (root.into_nodehash() != NULL_HASH).then_some((root, RepoPath::RootPath));
    Ok(CreateChangeset {
        expected_nodeid: None,
        expected_files: Some(files.iter().map(NonRootMPath::new).collect::<Result<_>>()?),
        p1: parents.next(),
        p2: parents.next(),
        subtree_changes: None,
        root_manifest: future::ok(root_manifest).boxed(),
        sub_entries: stream::empty::<Result<(Entry<HgManifestId, HgFileNodeId>, RepoPath)>>()
            .boxed(),
        cs_metadata: ChangesetMetadata {
            user: "author <author@fb.com>".into(),
            time: DateTime::from_timestamp(0, 0)?,
            extra: Default::default(),
            message: format!("commit with root {root}"),
        },
        upload_to_blobstore_only: false,
        acl_file_validation: None,
    }
    .create(
        ctx.clone(),
        repo,
        repo.restricted_paths_arc().config_based().clone(),
        None,
        MononokeScubaSampleBuilder::with_discard(),
    ))
}

async fn created(handle: &ChangesetHandle) -> Result<(ChangesetId, HgChangesetId)> {
    let (bonsai, hg_cs) = handle.clone().get_completed_changeset().await?;
    Ok((bonsai.get_changeset_id(), hg_cs.get_changeset_id()))
}

/// On top of an existing commit, upload a child that renames `a` and keeps
/// `dir`, an unrelated root, and a merge of the two that copies the renamed
/// file. With `block_hg_reads`, Hg manifest reads fail from the first
/// changeset creation on. Returns the Bonsai and Hg changeset ids created.
async fn upload_commits(
    fb: FacebookInit,
    block_hg_reads: bool,
) -> Result<(Vec<(ChangesetId, HgChangesetId)>, usize)> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing, existing_root) = existing_commit(&ctx, &repo, true).await?;
    let dir = existing_root
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            MPath::new("dir")?,
        )
        .await?
        .and_then(Entry::into_tree)
        .expect("setup: the existing commit has a directory `dir`");
    let a = upload_file(&ctx, &repo, "a", "a".to_string()).await?;

    let renamed = upload_file(
        &ctx,
        &repo,
        "renamed",
        format!("\x01\ncopy: a\ncopyrev: {a}\x01\na"),
    )
    .await?;
    let root_renamed = upload_root_tree(
        &ctx,
        &repo,
        format!("dir\0{dir}t\nrenamed\0{renamed}\n"),
        &[existing_root],
        true,
    )
    .await?;
    let other = upload_file(&ctx, &repo, "other", "other".to_string()).await?;
    let root_other = upload_root_tree(&ctx, &repo, format!("other\0{other}\n"), &[], true).await?;
    let copied = upload_file(
        &ctx,
        &repo,
        "copied",
        format!("\x01\ncopy: renamed\ncopyrev: {renamed}\x01\na"),
    )
    .await?;
    let root_merge = upload_root_tree(
        &ctx,
        &repo,
        format!("copied\0{copied}\ndir\0{dir}t\nother\0{other}\nrenamed\0{renamed}\n"),
        &[root_renamed, root_other],
        true,
    )
    .await?;

    if block_hg_reads {
        hg_reads.block();
    }
    let existing = ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing);
    let rename = create_changeset(&ctx, &repo, root_renamed, &["a", "renamed"], vec![existing])?;
    let other = create_changeset(&ctx, &repo, root_other, &["other"], vec![])?;
    let merge = create_changeset(
        &ctx,
        &repo,
        root_merge,
        &["copied"],
        vec![rename.clone(), other.clone()],
    )?;
    let created = vec![
        created(&rename).await?,
        created(&other).await?,
        created(&merge).await?,
    ];
    Ok((created, hg_reads.reads()))
}

#[mononoke::fbinit_test]
async fn test_commit_upload_reads_augmented_manifests(fb: FacebookInit) -> Result<()> {
    read_from_augmented(false);
    let (expected, _) = upload_commits(fb, false).await?;

    read_from_augmented(true);
    let (uploaded, hg_reads) = upload_commits(fb, true).await?;

    assert_eq!(
        uploaded, expected,
        "uploads create the same Bonsai and Hg changesets as when reading Hg manifests"
    );
    assert_eq!(hg_reads, 0, "uploads read no Hg manifest");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_commit_upload_reads_hg_manifests_with_knob_off(fb: FacebookInit) -> Result<()> {
    read_from_augmented(false);
    let result = upload_commits(fb, true).await;

    assert!(
        result.is_err(),
        "uploads still read Hg manifests with the knob off"
    );
    Ok(())
}

/// On top of an existing commit, upload a child that deletes every file, so
/// its root manifest is the null manifest, an unrelated root, and a merge of
/// the two that copies `other` from the unrelated root. With `block_hg_reads`,
/// Hg manifest reads fail from the first changeset creation on. Returns the
/// Bonsai and Hg changeset ids created and the number of Hg manifest reads.
async fn upload_null_manifest_commits(
    fb: FacebookInit,
    block_hg_reads: bool,
) -> Result<(Vec<(ChangesetId, HgChangesetId)>, usize)> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing, _) = existing_commit(&ctx, &repo, true).await?;
    let other = upload_file(&ctx, &repo, "other", "other".to_string()).await?;
    let root_other = upload_root_tree(&ctx, &repo, format!("other\0{other}\n"), &[], true).await?;
    let copied = upload_file(
        &ctx,
        &repo,
        "copied",
        format!("\x01\ncopy: other\ncopyrev: {other}\x01\nother"),
    )
    .await?;
    let root_merge = upload_root_tree(
        &ctx,
        &repo,
        format!("copied\0{copied}\nother\0{other}\n"),
        &[root_other],
        true,
    )
    .await?;

    if block_hg_reads {
        hg_reads.block();
    }
    let existing = ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing);
    let empty = create_changeset(
        &ctx,
        &repo,
        HgManifestId::new(NULL_HASH),
        &["a", "dir/b"],
        vec![existing],
    )?;
    let other = create_changeset(&ctx, &repo, root_other, &["other"], vec![])?;
    let merge = create_changeset(
        &ctx,
        &repo,
        root_merge,
        &["copied"],
        vec![empty.clone(), other.clone()],
    )?;
    let created = vec![
        created(&empty).await?,
        created(&other).await?,
        created(&merge).await?,
    ];
    Ok((created, hg_reads.reads()))
}

#[mononoke::fbinit_test]
async fn test_commit_upload_null_manifest_reads_augmented_manifests(
    fb: FacebookInit,
) -> Result<()> {
    read_from_augmented(false);
    let (expected, _) = upload_null_manifest_commits(fb, false).await?;

    read_from_augmented(true);
    let (uploaded, hg_reads) = upload_null_manifest_commits(fb, true).await?;

    assert_eq!(
        uploaded, expected,
        "uploads create the same Bonsai and Hg changesets as when reading Hg manifests"
    );
    assert_eq!(hg_reads, 0, "uploads read no Hg manifest");
    Ok(())
}

/// Upload a child of an existing commit, with Hg manifest reads blocked from
/// the changeset creation on, and return its creation error.
async fn upload_child_error(
    fb: FacebookInit,
    parent_augmented: bool,
    child_augmented: bool,
) -> Result<(String, HgManifestId, HgManifestId)> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing, existing_root) = existing_commit(&ctx, &repo, parent_augmented).await?;
    let c = upload_file(&ctx, &repo, "c", "c".to_string()).await?;
    let root = upload_root_tree(
        &ctx,
        &repo,
        format!("c\0{c}\n"),
        &[existing_root],
        child_augmented,
    )
    .await?;

    hg_reads.block();
    let existing = ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing);
    let child = create_changeset(&ctx, &repo, root, &["a", "c", "dir/b"], vec![existing])?;
    let err = created(&child)
        .await
        .expect_err("creating a changeset without the augmented manifests it needs fails");
    assert_eq!(hg_reads.reads(), 0, "the failed upload read no Hg manifest");
    Ok((format!("{err:#}"), existing_root, root))
}

#[mononoke::fbinit_test]
async fn test_commit_upload_uploaded_tree_without_augmented_manifest(
    fb: FacebookInit,
) -> Result<()> {
    read_from_augmented(true);
    let (err, _, root) = upload_child_error(fb, true, false).await?;

    let key = HgAugmentedManifestId::from(root).blobstore_key();
    assert!(
        err.contains(&key),
        "the error names the missing augmented manifest {key}: {err}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_commit_upload_parent_without_augmented_manifest(fb: FacebookInit) -> Result<()> {
    read_from_augmented(true);
    let (err, parent_root, _) = upload_child_error(fb, false, true).await?;

    let key = HgAugmentedManifestId::from(parent_root).blobstore_key();
    assert!(
        err.contains(&key),
        "the error names the missing augmented manifest {key}: {err}"
    );
    Ok(())
}
