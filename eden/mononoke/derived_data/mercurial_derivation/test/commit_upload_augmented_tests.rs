/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Commit upload with `scm/mononoke:hgmanifest_read_from_augmented` on reads
//! augmented manifests instead of Hg manifests.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
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
use mercurial_types::subtree::HgSubtreeChanges;
use mercurial_types::subtree::HgSubtreeDeepCopy;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::FileChange;
use mononoke_types::FileContents;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
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
/// in for a store where they have been deleted, counts reads of the shards
/// of sharded augmented directories, and fails reads of one chosen key.
#[derive(Debug, Default)]
struct BlockHgManifestReads {
    blocked: AtomicBool,
    reads: AtomicUsize,
    shard_reads: AtomicUsize,
    failing_key: Mutex<Option<String>>,
}

impl BlockHgManifestReads {
    fn check(&self, key: &str) -> Result<()> {
        if !self.blocked.load(Ordering::Relaxed) {
            return Ok(());
        }
        if key.contains(".hgaugmentedmanifest.map2node.") {
            self.shard_reads.fetch_add(1, Ordering::Relaxed);
        }
        // The repo blobstore prefixes its keys.
        if let Some(failing) = self.failing_key.lock().expect("lock poisoned").as_deref()
            && key.ends_with(failing)
        {
            anyhow::bail!("injected read failure of {key}");
        }
        if key.contains(".hgmanifest.sha1.") {
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

    /// Once blocked, fail every read of `key`.
    fn fail_reads_of(&self, key: String) {
        *self.failing_key.lock().expect("lock poisoned") = Some(key);
    }

    fn shard_reads(&self) -> usize {
        self.shard_reads.load(Ordering::Relaxed)
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
    let files = files.iter().map(NonRootMPath::new).collect::<Result<_>>()?;
    Ok(create_changeset_with_subtree_changes(
        ctx, repo, root, files, parents, None,
    ))
}

fn create_changeset_with_subtree_changes(
    ctx: &CoreContext,
    repo: &Repo,
    root: HgManifestId,
    files: Vec<NonRootMPath>,
    parents: Vec<ChangesetHandle>,
    subtree_changes: Option<(HgSubtreeChanges, HashMap<HgChangesetId, ChangesetHandle>)>,
) -> ChangesetHandle {
    let mut parents = parents.into_iter();
    // Commit upload sends no root tree for a commit with no files.
    let root_manifest = (root.into_nodehash() != NULL_HASH).then_some((root, RepoPath::RootPath));
    let cs_metadata = ChangesetMetadata {
        user: "author <author@fb.com>".into(),
        time: DateTime::from_timestamp(0, 0).expect("the epoch is a valid time"),
        extra: Default::default(),
        message: format!("commit with root {root}"),
    };
    CreateChangeset {
        expected_nodeid: None,
        expected_files: Some(files),
        p1: parents.next(),
        p2: parents.next(),
        subtree_changes,
        root_manifest: future::ok(root_manifest).boxed(),
        sub_entries: stream::empty::<Result<(Entry<HgManifestId, HgFileNodeId>, RepoPath)>>()
            .boxed(),
        cs_metadata,
        upload_to_blobstore_only: false,
        acl_file_validation: None,
    }
    .create(
        ctx.clone(),
        repo,
        repo.restricted_paths_arc().config_based().clone(),
        None,
        MononokeScubaSampleBuilder::with_discard(),
    )
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

// The file changes commit upload computes, for each shape of commit. Each test
// uploads the same commits twice, once reading Hg manifests and once reading
// augmented manifests with every Hg manifest read failing, and checks both
// against file changes written out by hand.

/// A file change as a test expects it.
enum Expected {
    /// Content, type, and the path and commit it was copied from.
    Changed(&'static str, FileType, Option<(&'static str, ChangesetId)>),
    Deleted,
}

fn expected_changes(
    changes: Vec<(&'static str, Expected)>,
) -> Result<BTreeMap<NonRootMPath, FileChange>> {
    changes
        .into_iter()
        .map(|(path, change)| {
            let change = match change {
                Expected::Changed(content, file_type, copy_from) => FileChange::tracked(
                    FileContents::content_id_for_bytes(&Bytes::from_static(content.as_bytes())),
                    file_type,
                    content.len() as u64,
                    copy_from
                        .map(|(from, cs_id)| anyhow::Ok((NonRootMPath::new(from)?, cs_id)))
                        .transpose()?,
                    GitLfs::FullContent,
                ),
                Expected::Deleted => FileChange::Deletion,
            };
            Ok((NonRootMPath::new(path)?, change))
        })
        .collect()
}

fn file_changes(bonsai: &BonsaiChangeset) -> BTreeMap<NonRootMPath, FileChange> {
    bonsai
        .file_changes()
        .map(|(path, change)| (path.clone(), change.clone()))
        .collect()
}

/// Derive `cs_id`'s Hg changeset and augmented manifests, then upload that Hg
/// changeset again as a client sends it: its root tree, its changed files and
/// its parents' Hg changesets. With `augmented`, Hg manifest reads fail from
/// the upload on. Returns the Bonsai changeset commit upload creates.
async fn reupload(
    ctx: &CoreContext,
    repo: &Repo,
    hg_reads: &BlockHgManifestReads,
    cs_id: ChangesetId,
    augmented: bool,
) -> Result<BonsaiChangeset> {
    let hg_cs_id = repo.derive_hg_changeset(ctx, cs_id).await?;
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await?;
    let hg_cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
    let mut parents = Vec::new();
    for parent in cs_id.load(ctx, repo.repo_blobstore()).await?.parents() {
        let parent = repo.derive_hg_changeset(ctx, parent).await?;
        parents.push(ChangesetHandle::ready_cs_handle(
            ctx.clone(),
            repo.clone(),
            parent,
        ));
    }

    if augmented {
        hg_reads.block();
    }
    let uploaded = create_changeset_with_subtree_changes(
        ctx,
        repo,
        hg_cs.manifestid(),
        hg_cs.files().to_vec(),
        parents,
        None,
    );
    let (bonsai, _) = uploaded.get_completed_changeset().await?;
    Ok(bonsai)
}

/// Build the commits of `case` in a fresh repo, upload the one it returns
/// again, and check the file changes, reading Hg manifests and then, in
/// another fresh repo, augmented manifests. Returns how many shards of
/// sharded augmented directories the augmented upload read.
async fn check_reupload(
    fb: FacebookInit,
    case: impl AsyncFn(CoreContext, Repo) -> Result<(ChangesetId, Vec<(&'static str, Expected)>)>,
) -> Result<usize> {
    let mut shard_reads = 0;
    for augmented in [false, true] {
        read_from_augmented(augmented);
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
        let (cs_id, expected) = case(ctx.clone(), repo.clone()).await?;
        let uploaded = reupload(&ctx, &repo, &hg_reads, cs_id, augmented).await?;
        assert_eq!(
            file_changes(&uploaded),
            expected_changes(expected)?,
            "file changes reading {} manifests",
            if augmented { "augmented" } else { "Hg" }
        );
        assert_eq!(hg_reads.reads(), 0, "the upload read no Hg manifest");
        shard_reads = hg_reads.shard_reads();
    }
    Ok(shard_reads)
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_merge(fb: FacebookInit) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        {
            let base = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("both", "base")
                .add_file("pick", "base")
                .add_file("src", "base")
                .commit()
                .await?;
            let p1 = CreateCommitContext::new(&ctx, &repo, vec![base])
                .add_file("from_p1", "1")
                .add_file("both", "p1")
                .add_file("pick", "pick1")
                .commit()
                .await?;
            let p2 = CreateCommitContext::new(&ctx, &repo, vec![base])
                .add_file("from_p2", "2")
                .add_file("both", "p2")
                .add_file("pick", "pick2")
                .add_file("src", "src2")
                .commit()
                .await?;
            // `both` changed on both sides gets new content, `pick` and `src`
            // take the second parent's version, `copied` copies a file only
            // the second parent has, `copied_src` copies the second parent's
            // version of a file both parents have, and `from_p1` and `from_p2`
            // come unchanged from one side each.
            let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
                .add_file("both", "merged")
                .add_file("pick", "pick2")
                .add_file("src", "src2")
                .add_file_with_copy_info("copied", "2", (p2, "from_p2"))
                .add_file_with_copy_info("copied_src", "src2", (p2, "src"))
                .commit()
                .await?;
            Ok((
                merge,
                vec![
                    ("both", Expected::Changed("merged", FileType::Regular, None)),
                    (
                        "copied",
                        Expected::Changed("2", FileType::Regular, Some(("from_p2", p2))),
                    ),
                    (
                        "copied_src",
                        Expected::Changed("src2", FileType::Regular, Some(("src", p2))),
                    ),
                    ("pick", Expected::Changed("pick2", FileType::Regular, None)),
                    ("src", Expected::Changed("src2", FileType::Regular, None)),
                ],
            ))
        }
    })
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_rename_and_a_copy(fb: FacebookInit) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        let base = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .add_file("dir/old", "old")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![base])
            .add_file_with_copy_info("a_copy", "a", (base, "a"))
            .add_file_with_copy_info("new/name", "old", (base, "dir/old"))
            .delete_file("dir/old")
            .commit()
            .await?;
        Ok((
            child,
            vec![
                (
                    "a_copy",
                    Expected::Changed("a", FileType::Regular, Some(("a", base))),
                ),
                ("dir/old", Expected::Deleted),
                (
                    "new/name",
                    Expected::Changed("old", FileType::Regular, Some(("dir/old", base))),
                ),
            ],
        ))
    })
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_deletions(fb: FacebookInit) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        {
            let base = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("top", "t")
                .add_file("dir/x", "x")
                .add_file("dir/sub/y", "y")
                .add_file("stay", "s")
                .commit()
                .await?;
            // A file and a whole directory.
            let child = CreateCommitContext::new(&ctx, &repo, vec![base])
                .delete_file("top")
                .delete_file("dir/x")
                .delete_file("dir/sub/y")
                .commit()
                .await?;
            Ok((
                child,
                vec![
                    ("dir/sub/y", Expected::Deleted),
                    ("dir/x", Expected::Deleted),
                    ("top", Expected::Deleted),
                ],
            ))
        }
    })
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_file_replaced_by_a_directory(
    fb: FacebookInit,
) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        let base = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("f", "f")
            .add_file("other", "o")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![base])
            .delete_file("f")
            .add_file("f/inner", "i")
            .commit()
            .await?;
        Ok((
            child,
            vec![
                ("f", Expected::Deleted),
                ("f/inner", Expected::Changed("i", FileType::Regular, None)),
            ],
        ))
    })
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_directory_replaced_by_a_file(
    fb: FacebookInit,
) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        {
            let base = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("d/x", "x")
                .add_file("d/y", "y")
                .add_file("other", "o")
                .commit()
                .await?;
            let child = CreateCommitContext::new(&ctx, &repo, vec![base])
                .add_file("d", "file")
                .commit()
                .await?;
            // The new file deletes the directory's files implicitly.
            Ok((
                child,
                vec![("d", Expected::Changed("file", FileType::Regular, None))],
            ))
        }
    })
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_an_empty_commit(fb: FacebookInit) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        let base = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![base])
            .commit()
            .await?;
        Ok((child, vec![]))
    })
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_type_change(fb: FacebookInit) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        {
            let base = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("script", "run")
                .add_file("link", "target")
                .commit()
                .await?;
            // Only the type changes: the Hg file versions are reused.
            let child = CreateCommitContext::new(&ctx, &repo, vec![base])
                .add_file_with_type("script", "run", FileType::Executable)
                .add_file_with_type("link", "target", FileType::Symlink)
                .commit()
                .await?;
            Ok((
                child,
                vec![
                    ("link", Expected::Changed("target", FileType::Symlink, None)),
                    (
                        "script",
                        Expected::Changed("run", FileType::Executable, None),
                    ),
                ],
            ))
        }
    })
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_in_a_sharded_directory(fb: FacebookInit) -> Result<()> {
    let shard_reads = check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        {
            // More entries than an augmented directory holds unsharded: the
            // entries starting `f1` go to a shard of their own, where this
            // commit changes and deletes a file.
            let files: Vec<_> = (0..2500)
                .map(|i| (format!("big/f{i:04}"), "n".to_string()))
                .map(|(path, content)| anyhow::Ok((NonRootMPath::new(path)?, content)))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .collect();
            let base = CreateCommitContext::new_root(&ctx, &repo)
                .add_files(files)
                .commit()
                .await?;
            let child = CreateCommitContext::new(&ctx, &repo, vec![base])
                .add_file("big/f1100", "changed")
                .delete_file("big/f1200")
                .add_file("big/new", "new")
                .commit()
                .await?;
            Ok((
                child,
                vec![
                    (
                        "big/f1100",
                        Expected::Changed("changed", FileType::Regular, None),
                    ),
                    ("big/f1200", Expected::Deleted),
                    ("big/new", Expected::Changed("new", FileType::Regular, None)),
                ],
            ))
        }
    })
    .await?;
    assert!(
        shard_reads > 0,
        "`big` is sharded in its augmented manifest"
    );
    Ok(())
}

/// A Bonsai changeset made on the server can list a file with the content it
/// already had. Its Hg changeset reuses the file's Hg version, so commit upload
/// of that Hg changeset lists no change there. This is expected: the upload
/// differs from the stored commit, whichever manifests it reads.
#[mononoke::fbinit_test]
async fn test_upload_file_changes_skip_a_file_listed_unchanged(fb: FacebookInit) -> Result<()> {
    check_reupload(fb, async |ctx: CoreContext, repo: Repo| {
        let base = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .add_file("same", "same")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![base])
            .add_file("a", "a2")
            .add_file("same", "same")
            .commit()
            .await?;
        let stored = child.load(&ctx, repo.repo_blobstore()).await?;
        assert!(
            stored
                .file_changes_map()
                .contains_key(&NonRootMPath::new("same")?),
            "setup: the stored commit lists `same`"
        );
        Ok((
            child,
            vec![("a", Expected::Changed("a2", FileType::Regular, None))],
        ))
    })
    .await?;
    Ok(())
}

/// Upload, on top of an existing commit, a child that adds `c` as a copy of
/// `a`, and a grandchild that only makes `c` executable, keeping its Hg file
/// version as Sapling does. Returns the grandchild's file changes.
async fn upload_copy_then_type_change(
    fb: FacebookInit,
    augmented: bool,
) -> Result<BTreeMap<NonRootMPath, FileChange>> {
    read_from_augmented(augmented);
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing, existing_root) = existing_commit(&ctx, &repo, true).await?;
    let a = existing_entry(&ctx, &repo, existing_root, "a")
        .await?
        .into_leaf()
        .expect("setup: `a` is a file")
        .1;
    let dir = existing_entry(&ctx, &repo, existing_root, "dir")
        .await?
        .into_tree()
        .expect("setup: `dir` is a directory");
    let c = upload_file(
        &ctx,
        &repo,
        "c",
        format!("\x01\ncopy: a\ncopyrev: {a}\x01\na"),
    )
    .await?;
    let root_copy = upload_root_tree(
        &ctx,
        &repo,
        format!("a\0{a}\nc\0{c}\ndir\0{dir}t\n"),
        &[existing_root],
        true,
    )
    .await?;
    let root_exec = upload_root_tree(
        &ctx,
        &repo,
        format!("a\0{a}\nc\0{c}x\ndir\0{dir}t\n"),
        &[root_copy],
        true,
    )
    .await?;

    if augmented {
        hg_reads.block();
    }
    let existing = ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing);
    let copy = create_changeset(&ctx, &repo, root_copy, &["c"], vec![existing])?;
    let exec = create_changeset(&ctx, &repo, root_exec, &["c"], vec![copy])?;
    let (bonsai, _) = exec.get_completed_changeset().await?;
    assert_eq!(hg_reads.reads(), 0, "the upload read no Hg manifest");
    Ok(file_changes(&bonsai))
}

/// A type change that keeps a file version carrying copy information lists
/// the file without copying it again.
#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_type_change_after_a_copy(fb: FacebookInit) -> Result<()> {
    for augmented in [false, true] {
        assert_eq!(
            upload_copy_then_type_change(fb, augmented).await?,
            expected_changes(vec![(
                "c",
                Expected::Changed("a", FileType::Executable, None)
            )])?,
            "reading augmented manifests: {augmented}"
        );
    }
    Ok(())
}

/// The entry at `path` in the existing commit's root tree `root`.
async fn existing_entry(
    ctx: &CoreContext,
    repo: &Repo,
    root: HgManifestId,
    path: &str,
) -> Result<Entry<HgManifestId, (FileType, HgFileNodeId)>> {
    Ok(root
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            MPath::new(path)?,
        )
        .await?
        .expect("setup: the existing commit has the entry"))
}

/// Upload, on top of an existing commit, a file whose copy information names
/// a version of `a` that no parent has, and return the creation error.
async fn upload_copy_from_missing_source(fb: FacebookInit, augmented: bool) -> Result<String> {
    read_from_augmented(augmented);
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing, existing_root) = existing_commit(&ctx, &repo, true).await?;
    let a = existing_entry(&ctx, &repo, existing_root, "a")
        .await?
        .into_leaf()
        .expect("setup: `a` is a file")
        .1;
    let dir = existing_entry(&ctx, &repo, existing_root, "dir")
        .await?
        .into_tree()
        .expect("setup: `dir` is a directory");
    let other_a = upload_file(&ctx, &repo, "a", "another a".to_string()).await?;
    let c = upload_file(
        &ctx,
        &repo,
        "c",
        format!("\x01\ncopy: a\ncopyrev: {other_a}\x01\nanother a"),
    )
    .await?;
    let root = upload_root_tree(
        &ctx,
        &repo,
        format!("a\0{a}\nc\0{c}\ndir\0{dir}t\n"),
        &[existing_root],
        true,
    )
    .await?;

    if augmented {
        hg_reads.block();
    }
    let existing = ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing);
    let child = create_changeset(&ctx, &repo, root, &["c"], vec![existing])?;
    let err = created(&child)
        .await
        .expect_err("a copy from a version no parent has is refused");
    assert_eq!(hg_reads.reads(), 0, "the upload read no Hg manifest");
    Ok(format!("{err:#}"))
}

#[mononoke::fbinit_test]
async fn test_upload_refuses_a_copy_from_a_missing_source(fb: FacebookInit) -> Result<()> {
    for augmented in [false, true] {
        let err = upload_copy_from_missing_source(fb, augmented).await?;
        assert!(
            err.contains("Incorrect copy info"),
            "reading augmented manifests: {augmented}, error: {err}"
        );
    }
    Ok(())
}

/// Upload, on top of an existing commit, a subtree copy of `dir` to `dir_copy`
/// that reuses `dir`'s tree, and return the uploaded Bonsai changeset.
async fn upload_subtree_copy(fb: FacebookInit, augmented: bool) -> Result<BonsaiChangeset> {
    read_from_augmented(augmented);
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing_hg, existing_root) = existing_commit(&ctx, &repo, true).await?;
    let a = existing_entry(&ctx, &repo, existing_root, "a")
        .await?
        .into_leaf()
        .expect("setup: `a` is a file")
        .1;
    let dir = existing_entry(&ctx, &repo, existing_root, "dir")
        .await?
        .into_tree()
        .expect("setup: `dir` is a directory");
    let root = upload_root_tree(
        &ctx,
        &repo,
        format!("a\0{a}\ndir\0{dir}t\ndir_copy\0{dir}t\n"),
        &[existing_root],
        true,
    )
    .await?;

    if augmented {
        hg_reads.block();
    }
    let existing = ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing_hg);
    let subtree_changes = HgSubtreeChanges {
        deep_copies: vec![HgSubtreeDeepCopy {
            from_commit: existing_hg,
            from_path: MPath::new("dir")?,
            to_path: MPath::new("dir_copy")?,
        }],
        ..Default::default()
    };
    let sources = HashMap::from([(existing_hg, existing.clone())]);
    let child = create_changeset_with_subtree_changes(
        &ctx,
        &repo,
        root,
        vec![NonRootMPath::new("dir_copy/b")?],
        vec![existing],
        Some((subtree_changes, sources)),
    );
    let (bonsai, _) = child.get_completed_changeset().await?;
    assert_eq!(hg_reads.reads(), 0, "the upload read no Hg manifest");
    Ok(bonsai)
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_subtree_copy(fb: FacebookInit) -> Result<()> {
    for augmented in [false, true] {
        let uploaded = upload_subtree_copy(fb, augmented).await?;
        assert_eq!(
            file_changes(&uploaded),
            expected_changes(vec![(
                "dir_copy/b",
                Expected::Changed("b", FileType::Regular, None)
            )])?,
            "the copied files are listed, without copy information; reading augmented manifests: {augmented}"
        );
        assert_eq!(
            uploaded.subtree_changes().len(),
            1,
            "the subtree copy is kept"
        );
    }
    Ok(())
}

/// Upload a commit with no files whose two parents share the file `a`, and
/// return its file changes. No client sends one, but SLAPI accepts it.
async fn upload_null_root_merge(
    fb: FacebookInit,
    augmented: bool,
) -> Result<BTreeMap<NonRootMPath, FileChange>> {
    read_from_augmented(augmented);
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing, _) = existing_commit(&ctx, &repo, true).await?;
    let other = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a", "other a")
        .add_file("x", "x")
        .commit()
        .await?;
    let other_hg = repo.derive_hg_changeset(&ctx, other).await?;
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(&ctx, other, DerivationPriority::LOW)
        .await?;

    if augmented {
        hg_reads.block();
    }
    let parents = vec![
        ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing),
        ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), other_hg),
    ];
    let merge = create_changeset(
        &ctx,
        &repo,
        HgManifestId::new(NULL_HASH),
        &["a", "dir/b", "x"],
        parents,
    )?;
    let (bonsai, _) = merge.get_completed_changeset().await?;
    assert_eq!(hg_reads.reads(), 0, "the upload read no Hg manifest");
    Ok(file_changes(&bonsai))
}

#[mononoke::fbinit_test]
async fn test_upload_file_changes_of_a_null_root_merge(fb: FacebookInit) -> Result<()> {
    for augmented in [false, true] {
        assert_eq!(
            upload_null_root_merge(fb, augmented).await?,
            expected_changes(vec![
                ("a", Expected::Deleted),
                ("dir/b", Expected::Deleted),
                ("x", Expected::Deleted),
            ])?,
            "every file of either parent is deleted once; reading augmented manifests: {augmented}"
        );
    }
    Ok(())
}

/// Upload a merge of a root commit with the file `a` and one with `a/b`,
/// keeping the second parent's `a` and copying `a/b` to `c`. The copy source's
/// path runs through a file in the first parent. Returns the merge's file
/// changes and the second parent's Bonsai changeset id.
async fn upload_copy_through_a_file_in_a_parent(
    fb: FacebookInit,
    augmented: bool,
) -> Result<(BTreeMap<NonRootMPath, FileChange>, ChangesetId)> {
    read_from_augmented(augmented);
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let mut parents = Vec::new();
    for files in [vec![("a", "file a")], vec![("a/b", "ab"), ("other", "o")]] {
        let cs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_files(files)
            .commit()
            .await?;
        let hg_cs_id = repo.derive_hg_changeset(&ctx, cs_id).await?;
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&ctx, cs_id, DerivationPriority::LOW)
            .await?;
        let root = hg_cs_id
            .load(&ctx, repo.repo_blobstore())
            .await?
            .manifestid();
        parents.push((cs_id, hg_cs_id, root));
    }
    let [(_, p1_hg, p1_root), (p2, p2_hg, p2_root)] = parents[..] else {
        unreachable!("two parents were created");
    };
    let a_dir = existing_entry(&ctx, &repo, p2_root, "a")
        .await?
        .into_tree()
        .expect("setup: `a` is a directory in the second parent");
    let ab = existing_entry(&ctx, &repo, p2_root, "a/b")
        .await?
        .into_leaf()
        .expect("setup: `a/b` is a file in the second parent")
        .1;
    let other = existing_entry(&ctx, &repo, p2_root, "other")
        .await?
        .into_leaf()
        .expect("setup: `other` is a file in the second parent")
        .1;
    let c = upload_file(
        &ctx,
        &repo,
        "c",
        format!("\x01\ncopy: a/b\ncopyrev: {ab}\x01\nab"),
    )
    .await?;
    let root = upload_root_tree(
        &ctx,
        &repo,
        format!("a\0{a_dir}t\nc\0{c}\nother\0{other}\n"),
        &[p1_root, p2_root],
        true,
    )
    .await?;

    if augmented {
        hg_reads.block();
    }
    let parents = [p1_hg, p2_hg]
        .into_iter()
        .map(|hg| ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), hg))
        .collect();
    let merge = create_changeset(&ctx, &repo, root, &["a", "c"], parents)?;
    let (bonsai, _) = merge.get_completed_changeset().await?;
    assert_eq!(hg_reads.reads(), 0, "the upload read no Hg manifest");
    Ok((file_changes(&bonsai), p2))
}

/// A copy source whose path runs through a file in one parent is not in that
/// parent, and is found in the other, on both paths.
#[mononoke::fbinit_test]
async fn test_upload_copy_from_a_path_through_a_file_in_a_parent(fb: FacebookInit) -> Result<()> {
    for augmented in [false, true] {
        let (uploaded, p2) = upload_copy_through_a_file_in_a_parent(fb, augmented).await?;
        assert_eq!(
            uploaded,
            expected_changes(vec![
                ("a", Expected::Deleted),
                (
                    "c",
                    Expected::Changed("ab", FileType::Regular, Some(("a/b", p2)))
                ),
            ])?,
            "the file `a` is deleted, `a/b` comes unchanged from the second parent, and `c` is copied from it; reading augmented manifests: {augmented}"
        );
    }
    Ok(())
}

/// With the switch on, a read error while looking up a copy source fails the
/// upload, rather than being taken for a source the parent does not have.
#[mononoke::fbinit_test]
async fn test_upload_fails_on_a_copy_source_read_error(fb: FacebookInit) -> Result<()> {
    read_from_augmented(true);
    let ctx = CoreContext::test_mock(fb);
    let (repo, hg_reads) = repo_with_blockable_hg_reads(fb).await?;
    let (existing, existing_root) = existing_commit(&ctx, &repo, true).await?;
    let a = existing_entry(&ctx, &repo, existing_root, "a")
        .await?
        .into_leaf()
        .expect("setup: `a` is a file")
        .1;
    let dir = existing_entry(&ctx, &repo, existing_root, "dir")
        .await?
        .into_tree()
        .expect("setup: `dir` is a directory");
    let b = existing_entry(&ctx, &repo, existing_root, "dir/b")
        .await?
        .into_leaf()
        .expect("setup: `dir/b` is a file")
        .1;
    let c = upload_file(
        &ctx,
        &repo,
        "c",
        format!("\x01\ncopy: dir/b\ncopyrev: {b}\x01\nb"),
    )
    .await?;
    let root = upload_root_tree(
        &ctx,
        &repo,
        format!("a\0{a}\nc\0{c}\ndir\0{dir}t\n"),
        &[existing_root],
        true,
    )
    .await?;

    // `dir` is unchanged, so only the copy lookup reads its augmented record.
    let dir_key = HgAugmentedManifestId::from(dir).blobstore_key();
    hg_reads.fail_reads_of(dir_key.clone());
    hg_reads.block();
    let existing = ChangesetHandle::ready_cs_handle(ctx.clone(), repo.clone(), existing);
    let child = create_changeset(&ctx, &repo, root, &["c"], vec![existing])?;
    let err = format!(
        "{:#}",
        created(&child)
            .await
            .expect_err("a failed copy-source read fails the upload")
    );
    assert!(
        err.contains("While fetching copy information") && err.contains(&dir_key),
        "the error says the copy lookup failed reading {dir_key}: {err}"
    );
    assert_eq!(hg_reads.reads(), 0, "the upload read no Hg manifest");
    Ok(())
}
