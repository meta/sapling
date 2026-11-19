/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Conversion of snapshots uploaded as hg commits into snapshot bonsais, and
//! storage of their objects in a bubble.

use std::collections::BTreeSet;
use std::collections::HashSet;

use anyhow::anyhow;
use blobrepo_hg::create_bonsai_changeset_object;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use bonsai_hg_mapping::BonsaiHgMappingEntry;
use bonsai_hg_mapping::BonsaiHgMappingError;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bytes::Bytes;
use context::CoreContext;
use ephemeral_blobstore::BubbleId;
use ephemeral_blobstore::EphemeralBlobstoreError;
use ephemeral_blobstore::bubble_hg_mapping_enabled;
use filestore::FetchKey;
use filestore::FilestoreConfig;
use filestore::FilestoreConfigRef;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use manifest::Entry;
use manifest::ManifestOps;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestEnvelope;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::blobs::ContentBlobMeta;
use mercurial_types::blobs::File;
use mercurial_types::blobs::HgBlobChangeset;
use mercurial_types::blobs::HgChangesetContent;
use mercurial_types::blobs::MononokeHgBlobError;
use mercurial_types::blobs::UploadHgFileContents;
use mercurial_types::blobs::UploadHgFileEntry;
use mercurial_types::blobs::UploadHgNodeHash;
use mercurial_types::blobs::snapshot_state;
use mercurial_types::blobs::store_manifest_envelope;
use mononoke_api::MononokeRepo;
use mononoke_types::BlobstoreKey;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::FileChange;
use mononoke_types::FileContents;
use mononoke_types::NonRootMPath;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use thiserror::Error;

use crate::HgRepoContext;

/// An error from turning an uploaded snapshot into Mononoke objects.
#[derive(Debug, Error)]
pub enum SnapshotUploadError {
    /// The client sent something that is not a valid snapshot, or a snapshot
    /// that refers to objects that are neither in the request nor stored.
    #[error("Invalid snapshot: {0:#}")]
    InvalidSnapshot(anyhow::Error),
    /// The snapshot's hg commit is already mapped in another bubble, for
    /// example by an identical upload that finished first. Nothing refers to
    /// the objects this request stored.
    #[error("the snapshot's hg commit is already in bubble {0}")]
    InAnotherBubble(BubbleId),
    /// A storage or other server failure. Conversion failures that the checks
    /// for an invalid snapshot do not catch are reported here too, for example
    /// copy information whose source is in no parent.
    #[error(transparent)]
    Internal(anyhow::Error),
}

/// A snapshot as the client sent it, to be stored in a bubble.
///
/// The client may leave out objects of the parent commits and objects it sent
/// with an earlier snapshot that was stored in the same bubble. A request that
/// fails can leave some of its objects in the bubble without the objects they
/// refer to, so the client does not leave out objects it sent with a request
/// that failed.
#[derive(Clone, Debug)]
pub struct SnapshotUpload {
    /// The hg changeset id the client computed and sent.
    pub hg_changeset_id: HgChangesetId,
    /// The snapshot's hg changeset as the client sent it. Its id is computed
    /// from it and checked against `hg_changeset_id`.
    pub changeset: HgChangesetContent,
    /// The trees the snapshot adds.
    pub trees: Vec<SnapshotTree>,
    /// The filenodes the snapshot adds.
    pub filenodes: Vec<SnapshotFilenode>,
}

/// A tree of a snapshot, in the hg manifest format.
#[derive(Clone, Debug)]
pub struct SnapshotTree {
    /// The node id the client sent, checked against `data` and the parents.
    pub node_id: HgNodeHash,
    pub p1: Option<HgNodeHash>,
    pub p2: Option<HgNodeHash>,
    /// The tree's text: one `name\0<hex id><flag>\n` line per entry.
    pub data: Bytes,
}

/// A filenode of a snapshot.
#[derive(Clone, Debug)]
pub struct SnapshotFilenode {
    /// The node id the client sent, checked against the metadata, the
    /// contents and the parents.
    pub node_id: HgFileNodeId,
    pub p1: Option<HgFileNodeId>,
    pub p2: Option<HgFileNodeId>,
    /// The hg filelog metadata header, such as copy information, or empty if
    /// there is none.
    pub metadata: Bytes,
    pub contents: SnapshotFileContents,
}

/// Where the contents of a snapshot's file come from.
#[derive(Clone, Debug)]
pub enum SnapshotFileContents {
    /// Sent in the request.
    Inline(Bytes),
    /// Uploaded beforehand, to the bubble or to permanent storage.
    Uploaded(ContentId),
}

/// A snapshot stored in a bubble.
#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    /// The snapshot's hg commit, stored in the bubble.
    pub hg_changeset: HgBlobChangeset,
    /// The snapshot bonsai, saved in the bubble's commit graph.
    pub bonsai: BonsaiChangeset,
}

impl<R: MononokeRepo> HgRepoContext<R> {
    /// Stores a snapshot in the bubble this context's repo was opened in.
    ///
    /// The hg commit, trees and filenodes are checked against their ids and
    /// stored. Building the snapshot's bonsai then loads every tree and
    /// filenode that differs from the parents', so it fails if the request
    /// leaves out one that is not stored, and, as in `upload/changesets`, the
    /// contents of every changed file must be stored. The bubble's commit and
    /// bonsai<->hg mapping row are written last, the row only while
    /// `scm/mononoke:enable_bubble_hg_mapping` is on for the repo, so a request
    /// that fails leaves only objects that nothing refers to.
    pub async fn store_snapshot(
        &self,
        upload: SnapshotUpload,
    ) -> Result<StoredSnapshot, SnapshotUploadError> {
        let ctx = self.ctx();
        let repo = self.repo();
        let blobstore = repo.repo_blobstore();
        if repo.repo_derived_data().manager().bubble_id().is_none() {
            return Err(SnapshotUploadError::Internal(anyhow!(
                "snapshots can only be stored in a repo opened in a bubble"
            )));
        }
        let SnapshotUpload {
            hg_changeset_id,
            changeset,
            trees,
            filenodes,
        } = upload;

        let changeset = HgBlobChangeset::new(changeset)
            .map_err(|err| invalid(err.context("the hg changeset cannot be serialized")))?;
        let computed_id = changeset.get_changeset_id();
        if computed_id != hg_changeset_id {
            return Err(invalid(anyhow!(
                "the hg changeset id {hg_changeset_id} does not match the changeset, whose id is {computed_id}"
            )));
        }
        snapshot_state(changeset.extra())
            .map_err(invalid)?
            .ok_or_else(|| {
                invalid(anyhow!(
                    "not a snapshot: the hg commit has no snapshot extra"
                ))
            })?;
        let mut bonsai_parents = Vec::new();
        let mut parent_manifests = Vec::new();
        for parent in &changeset.parents() {
            let parent = HgChangesetId::new(parent);
            let bonsai = repo
                .bonsai_hg_mapping()
                .get_bonsai_from_hg(ctx, parent)
                .await
                .map_err(SnapshotUploadError::Internal)?
                .ok_or_else(|| invalid(anyhow!("the parent commit {parent} is not stored")))?;
            let parent_cs = parent
                .load(ctx, blobstore)
                .await
                .map_err(|err| SnapshotUploadError::Internal(err.into()))?;
            bonsai_parents.push(bonsai);
            parent_manifests.push(parent_cs.manifestid());
        }
        let trees: Vec<HgManifestEnvelope> = stream::iter(trees)
            .then(|tree| async move {
                self.verify_tree(tree.node_id, tree.p1, tree.p2, tree.data, None)
                    .await
                    .map_err(|err| invalid(err.into()))
            })
            .try_collect()
            .await?;

        let filestore_config = *repo.filestore_config();
        let stored_contents: HashSet<ContentId> = stream::iter(filenodes)
            .map(|filenode| store_filenode(ctx, blobstore, filestore_config, filenode))
            .buffer_unordered(100)
            .try_collect()
            .await?;
        stream::iter(trees.iter().cloned())
            .map(|envelope| store_manifest_envelope(ctx, blobstore, envelope))
            .buffer_unordered(100)
            .try_collect::<Vec<_>>()
            .await
            .map_err(SnapshotUploadError::Internal)?;
        changeset
            .save(ctx, blobstore)
            .await
            .map_err(SnapshotUploadError::Internal)?;

        let bonsai = snapshot_bonsai_from_hg(
            ctx,
            blobstore,
            changeset.clone(),
            parent_manifests,
            bonsai_parents,
        )
        .await
        .map_err(|err| match err {
            SnapshotUploadError::Internal(err) if is_missing_blob(&err) => invalid(err.context(
                "the snapshot refers to an object that is neither in the request nor stored",
            )),
            err => err,
        })?;
        // A filenode that the request leaves out may come from a failed request
        // that stored it without its contents.
        let missing_contents: BTreeSet<ContentId> = stream::iter(
            bonsai
                .file_changes()
                .filter_map(|(_, change)| change.simplify())
                .map(|change| change.content_id())
                .filter(|content_id| !stored_contents.contains(content_id))
                .collect::<HashSet<_>>(),
        )
        .map(|content_id| async move {
            let present = blobstore
                .is_present(ctx, &content_id.blobstore_key())
                .await?
                .fail_if_unsure()?;
            anyhow::Ok((!present).then_some(content_id))
        })
        .buffer_unordered(100)
        .try_filter_map(future::ok)
        .try_collect()
        .await
        .map_err(SnapshotUploadError::Internal)?;
        if !missing_contents.is_empty() {
            let missing_contents: Vec<String> =
                missing_contents.iter().map(ContentId::to_string).collect();
            return Err(invalid(anyhow!(
                "the contents {} of changed files are neither in the request nor stored",
                missing_contents.join(", ")
            )));
        }

        if let Err(err) = self
            .build_and_store_augmented_manifests_for_uploaded_trees(trees)
            .await
        {
            ctx.scuba().clone().log_with_msg(
                "Failed to build augmented Hg manifests for a snapshot",
                Some(format!("hg_cs_id: {hg_changeset_id}, err: {err:#}")),
            );
        }

        changesets_creation::save_changesets(ctx, repo, vec![bonsai.clone()])
            .await
            .map_err(SnapshotUploadError::Internal)?;

        if bubble_hg_mapping_enabled(repo.repo_identity().name()) {
            repo.bonsai_hg_mapping()
                .add(
                    ctx,
                    BonsaiHgMappingEntry {
                        hg_cs_id: hg_changeset_id,
                        bcs_id: bonsai.get_changeset_id(),
                    },
                )
                .await
                .map_err(mapping_error)?;
        }

        Ok(StoredSnapshot {
            hg_changeset: changeset,
            bonsai,
        })
    }
}

/// Stores a filenode and, for inline contents, its contents, after checking
/// its hash. Returns the id of its contents.
async fn store_filenode(
    ctx: &CoreContext,
    blobstore: &RepoBlobstore,
    filestore_config: FilestoreConfig,
    filenode: SnapshotFilenode,
) -> Result<ContentId, SnapshotUploadError> {
    let SnapshotFilenode {
        node_id,
        p1,
        p2,
        metadata,
        contents,
    } = filenode;
    let copy_from = File::extract_copied_from(&metadata).map_err(|err| {
        invalid(err.context(format!("filenode {node_id} has invalid copy metadata")))
    })?;
    // As in `upload/filenodes`, a copied filenode keeps its parent in p2.
    let (p1, p2) = match copy_from {
        Some(_) if p2.is_some() => {
            return Err(invalid(anyhow!(
                "copy metadata is not valid for merged filenode {node_id}"
            )));
        }
        Some(_) => (None, p1),
        None => (p1, p2),
    };
    let (content_id, contents) = match contents {
        SnapshotFileContents::Inline(bytes) => {
            let raw = Bytes::from([metadata.as_ref(), bytes.as_ref()].concat());
            // Without `metadata`, contents that start with a metadata header
            // are stored without it.
            let stored = File::new(raw.clone(), p1, p2).file_contents().into_bytes();
            (
                FileContents::content_id_for_bytes(&stored),
                UploadHgFileContents::RawBytes(raw, filestore_config),
            )
        }
        SnapshotFileContents::Uploaded(content_id) => {
            let size = filestore::get_metadata(blobstore, ctx, &FetchKey::Canonical(content_id))
                .await
                .map_err(SnapshotUploadError::Internal)?
                .ok_or_else(|| {
                    invalid(anyhow!(
                        "filenode {node_id} names contents {content_id} that are not stored"
                    ))
                })?
                .total_size;
            (
                content_id,
                UploadHgFileContents::ContentUploaded(ContentBlobMeta {
                    id: content_id,
                    size,
                    copy_from,
                }),
            )
        }
    };
    UploadHgFileEntry {
        upload_node_id: UploadHgNodeHash::Checked(node_id.into_nodehash()),
        contents,
        p1,
        p2,
    }
    .upload(ctx.clone(), blobstore.boxed(), None)
    .await
    .map_err(|err| match err.downcast_ref::<MononokeHgBlobError>() {
        Some(
            MononokeHgBlobError::InconsistentEntryHash(..)
            | MononokeHgBlobError::InconsistentEntryHashForPath(..),
        ) => invalid(err),
        _ => SnapshotUploadError::Internal(err),
    })?;
    Ok(content_id)
}

/// Classifies a failure to write a snapshot's bonsai<->hg mapping row.
fn mapping_error(err: anyhow::Error) -> SnapshotUploadError {
    if let Some(EphemeralBlobstoreError::HgChangesetInAnotherBubble(_, bubble_id)) =
        err.downcast_ref()
    {
        return SnapshotUploadError::InAnotherBubble(*bubble_id);
    }
    match err.downcast_ref::<BonsaiHgMappingError>() {
        // The bonsai does not record everything in the hg commit, such as its
        // list of changed files, so two hg commits can have the same bonsai.
        Some(BonsaiHgMappingError::ConflictingEntries(stored, attempted))
            if stored.bcs_id == attempted.bcs_id =>
        {
            invalid(anyhow!(
                "the snapshot's bonsai {} is already stored as hg commit {}",
                stored.bcs_id,
                stored.hg_cs_id
            ))
        }
        _ => SnapshotUploadError::Internal(err),
    }
}

/// Whether `err` comes from loading a blob that is not stored.
fn is_missing_blob(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<LoadableError>(),
            Some(LoadableError::Missing(_))
        )
    })
}

/// Builds the snapshot bonsai for a snapshot hg commit, whose manifest is the
/// working copy including untracked files and excluding missing files.
///
/// The untracked and missing paths come from the commit's `snapshot` extra and
/// become untracked changes and untracked deletions. The hg extras, including
/// `snapshot`, are kept. The commit's trees and filenodes and the parents'
/// trees must be readable from `blobstore`.
pub async fn snapshot_bonsai_from_hg(
    ctx: &CoreContext,
    blobstore: &RepoBlobstore,
    hg_cs: HgBlobChangeset,
    parent_manifests: Vec<HgManifestId>,
    bonsai_parents: Vec<ChangesetId>,
) -> Result<BonsaiChangeset, SnapshotUploadError> {
    let state = snapshot_state(hg_cs.extra())
        .map_err(SnapshotUploadError::InvalidSnapshot)?
        .ok_or_else(|| {
            invalid(anyhow!(
                "not a snapshot: the hg commit has no snapshot extra"
            ))
        })?;
    // `create_bonsai_changeset_object` would fail on these with an internal
    // error.
    std::str::from_utf8(hg_cs.user())
        .map_err(|err| invalid(anyhow!(err).context("the author is not valid UTF-8")))?;
    if hg_cs.user().contains(&b'\n') {
        return Err(invalid(anyhow!("the author contains a newline")));
    }
    std::str::from_utf8(hg_cs.message())
        .map_err(|err| invalid(anyhow!(err).context("the message is not valid UTF-8")))?;
    for key in hg_cs.extra().keys() {
        std::str::from_utf8(key).map_err(|err| {
            invalid(anyhow!(err).context(format!("the extra key {key:?} is not valid UTF-8")))
        })?;
    }
    let manifest_id = hg_cs.manifestid();
    // The snapshot's augmented manifests may not exist yet.
    let read_from_augmented = false;
    let mut bcs = create_bonsai_changeset_object(
        ctx,
        hg_cs,
        parent_manifests,
        bonsai_parents,
        Default::default(),
        blobstore,
        read_from_augmented,
    )
    .await
    .map_err(SnapshotUploadError::Internal)?
    .into_mut();

    let file_changes = &mut bcs.file_changes;
    let missing: HashSet<&NonRootMPath> = state.missing.iter().collect();
    for path in &state.untracked {
        if missing.contains(path) {
            return Err(invalid(anyhow!("{path} is both untracked and missing")));
        }
        let untracked = match file_changes.get(path) {
            Some(FileChange::Change(change)) if change.copy_from().is_none() => {
                FileChange::untracked(change.content_id(), change.file_type(), change.size())
            }
            _ => {
                return Err(invalid(anyhow!(
                    "untracked file {path} is not a new or changed file without copy information in the snapshot"
                )));
            }
        };
        file_changes.insert(path.clone(), untracked);
    }

    // A missing path with no change was added and then deleted from disk, so
    // it must be absent from the snapshot's manifest as well as the parents'.
    let mut added_then_missing = Vec::new();
    for path in state.missing {
        match file_changes.get(&path) {
            Some(FileChange::Deletion) => {
                file_changes.insert(path, FileChange::UntrackedDeletion);
            }
            None => added_then_missing.push(path),
            Some(_) => {
                return Err(invalid(anyhow!(
                    "missing file {path} is changed in the snapshot"
                )));
            }
        }
    }
    if !added_then_missing.is_empty() {
        let present = manifest_id
            .find_entries(ctx.clone(), blobstore.clone(), added_then_missing.clone())
            // A directory at a missing path holds untracked files.
            .try_filter(|(_, entry)| future::ready(matches!(entry, Entry::Leaf(_))))
            .try_next()
            .await
            .map_err(SnapshotUploadError::Internal)?;
        if let Some((path, _)) = present {
            return Err(invalid(anyhow!(
                "missing file {path} is present in the snapshot's manifest"
            )));
        }
        file_changes.extend(
            added_then_missing
                .into_iter()
                .map(|path| (path, FileChange::UntrackedDeletion)),
        );
    }

    bcs.is_snapshot = true;
    bcs.freeze().map_err(invalid)
}

fn invalid(err: anyhow::Error) -> SnapshotUploadError {
    SnapshotUploadError::InvalidSnapshot(err)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use anyhow::Result;
    use blobstore::KeyedBlobstore;
    use blobstore::Loadable;
    use bonsai_hg_mapping::BonsaiHgMappingRef;
    use bytes::Bytes;
    use commit_graph::CommitGraphRef;
    use derivation_queue_thrift::DerivationPriority;
    use ephemeral_blobstore::BubbleId;
    use ephemeral_blobstore::RepoEphemeralStoreRef;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfigRef;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use maplit::hashmap;
    use mercurial_derivation::DeriveHgChangeset;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mercurial_types::HgAugmentedManifestId;
    use mercurial_types::HgBlobNode;
    use mercurial_types::HgFileEnvelopeMut;
    use mercurial_types::HgParents;
    use mercurial_types::blobs::ChangesetMetadata;
    use mercurial_types::blobs::ManifestContent;
    use mercurial_types::blobs::SNAPSHOT_METADATA_KEY;
    use mercurial_types::calculate_hg_node_id;
    use mononoke_api::repo::Repo;
    use mononoke_api::repo::RepoContext;
    use mononoke_api::repo::RepoWithBubble;
    use mononoke_macros::mononoke;
    use mononoke_types::BlobstoreKey;
    use mononoke_types::DateTime;
    use mononoke_types::FileContents;
    use repo_blobstore::RepoBlobstoreRef;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;

    use super::*;
    use crate::RepoContextHgExt;

    const AUTHOR: &str = "Alice <alice@example.com>";
    const MESSAGE: &str = "snapshot";

    struct Fixture {
        ctx: CoreContext,
        repo: Repo,
        parent: ChangesetId,
        parent_hg: HgBlobChangeset,
        /// A commit of the working copy with untracked files added, whose hg
        /// manifest is the snapshot's manifest.
        working_copy: ChangesetId,
        snapshot_manifest: HgManifestId,
    }

    async fn fixture(fb: FacebookInit) -> Result<Fixture> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("modified", "old")
            .add_file("removed", "removed")
            .add_file("missing", "missing")
            .add_file("unchanged", "unchanged")
            .commit()
            .await?;
        let working_copy = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("modified", "new")
            .add_file("added", "added")
            .add_file("untracked", "untracked contents")
            .delete_file("removed")
            .delete_file("missing")
            .commit()
            .await?;
        let parent_hg = load_hg_changeset(&ctx, &repo, parent).await?;
        let snapshot_manifest = load_hg_changeset(&ctx, &repo, working_copy)
            .await?
            .manifestid();
        Ok(Fixture {
            ctx,
            repo,
            parent,
            parent_hg,
            working_copy,
            snapshot_manifest,
        })
    }

    async fn load_hg_changeset(
        ctx: &CoreContext,
        repo: &Repo,
        cs_id: ChangesetId,
    ) -> Result<HgBlobChangeset> {
        Ok(repo
            .derive_hg_changeset(ctx, cs_id)
            .await?
            .load(ctx, repo.repo_blobstore())
            .await?)
    }

    fn snapshot_extra(untracked: &[&str], missing: &[&str]) -> Result<BTreeMap<Bytes, Bytes>> {
        let mut metadata = changeset_metadata(BTreeMap::new())?;
        metadata.record_snapshot_state(
            untracked
                .iter()
                .map(NonRootMPath::new)
                .collect::<Result<Vec<_>>>()?,
            missing
                .iter()
                .map(NonRootMPath::new)
                .collect::<Result<Vec<_>>>()?,
        )?;
        Ok(metadata.extra)
    }

    fn changeset_metadata(extra: BTreeMap<Bytes, Bytes>) -> Result<ChangesetMetadata> {
        Ok(ChangesetMetadata {
            user: AUTHOR.to_string(),
            time: DateTime::from_timestamp(1_700_000_000, 3600)?,
            extra,
            message: MESSAGE.to_string(),
        })
    }

    fn snapshot_hg_cs(fixture: &Fixture, extra: BTreeMap<Bytes, Bytes>) -> Result<HgBlobChangeset> {
        snapshot_hg_cs_with_metadata(fixture, changeset_metadata(extra)?)
    }

    fn snapshot_hg_cs_with_metadata(
        fixture: &Fixture,
        metadata: ChangesetMetadata,
    ) -> Result<HgBlobChangeset> {
        let files = ["added", "missing", "modified", "removed", "untracked"]
            .iter()
            .map(NonRootMPath::new)
            .collect::<Result<Vec<_>>>()?;
        HgBlobChangeset::new(HgChangesetContent::new_from_parts(
            HgParents::new(
                Some(fixture.parent_hg.get_changeset_id().into_nodehash()),
                None,
            ),
            fixture.snapshot_manifest,
            metadata,
            files,
        ))
    }

    async fn convert(
        fixture: &Fixture,
        hg_cs: HgBlobChangeset,
    ) -> Result<BonsaiChangeset, SnapshotUploadError> {
        snapshot_bonsai_from_hg(
            &fixture.ctx,
            fixture.repo.repo_blobstore(),
            hg_cs,
            vec![fixture.parent_hg.manifestid()],
            vec![fixture.parent],
        )
        .await
    }

    fn assert_invalid<T: std::fmt::Debug>(result: Result<T, SnapshotUploadError>, expected: &str) {
        match result {
            Err(SnapshotUploadError::InvalidSnapshot(err)) => {
                let message = format!("{err:#}");
                assert!(
                    message.contains(expected),
                    "expected {expected:?} in {message:?}"
                );
            }
            other => panic!("expected an invalid snapshot error, got {other:?}"),
        }
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        let extra = snapshot_extra(&["untracked"], &["missing", "added_then_missing"])?;
        let hg_cs = snapshot_hg_cs(&fixture, extra.clone())?;

        let bcs = convert(&fixture, hg_cs.clone()).await?;

        let working_copy = fixture
            .working_copy
            .load(&fixture.ctx, fixture.repo.repo_blobstore())
            .await?;
        let working_copy_changes = working_copy.file_changes_map();
        let changes = bcs.file_changes_map();
        let change = |path: &str| -> Result<Option<&FileChange>> {
            Ok(changes.get(&NonRootMPath::new(path)?))
        };
        let working_copy_change = |path: &str| -> Result<Option<&FileChange>> {
            Ok(working_copy_changes.get(&NonRootMPath::new(path)?))
        };

        assert!(matches!(change("modified")?, Some(FileChange::Change(_))));
        assert_eq!(change("modified")?, working_copy_change("modified")?);
        assert!(matches!(change("added")?, Some(FileChange::Change(_))));
        assert_eq!(change("added")?, working_copy_change("added")?);
        assert_eq!(change("removed")?, Some(&FileChange::Deletion));
        let untracked = working_copy_change("untracked")?
            .and_then(FileChange::simplify)
            .ok_or_else(|| anyhow!("untracked has no contents in the working copy"))?;
        assert_eq!(
            change("untracked")?,
            Some(&FileChange::untracked(
                untracked.content_id(),
                untracked.file_type(),
                "untracked contents".len() as u64,
            ))
        );
        assert_eq!(change("missing")?, Some(&FileChange::UntrackedDeletion));
        assert_eq!(
            change("added_then_missing")?,
            Some(&FileChange::UntrackedDeletion)
        );
        assert_eq!(change("unchanged")?, None);
        assert_eq!(changes.len(), 6);

        assert!(bcs.is_snapshot());
        let expected_extra = extra
            .iter()
            .map(|(key, value)| Ok((std::str::from_utf8(key)?, value.as_ref())))
            .collect::<Result<Vec<(&str, &[u8])>, std::str::Utf8Error>>()?;
        assert_eq!(bcs.hg_extra().collect::<Vec<_>>(), expected_extra);
        assert_eq!(bcs.message(), MESSAGE);
        assert_eq!(bcs.author(), AUTHOR);
        assert_eq!(bcs.author_date(), hg_cs.time());
        assert_eq!(bcs.parents().collect::<Vec<_>>(), vec![fixture.parent]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_without_extra(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        let hg_cs = snapshot_hg_cs(&fixture, BTreeMap::new())?;
        assert_invalid(convert(&fixture, hg_cs).await, "not a snapshot");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_bad_extra(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        for (value, expected) in [
            ("not json", "Failed to parse snapshot state"),
            (
                r#"{"v":2,"untracked":[],"missing":[]}"#,
                "Unsupported version of snapshot state: 2",
            ),
        ] {
            let extra = BTreeMap::from([(
                Bytes::from_static(SNAPSHOT_METADATA_KEY),
                Bytes::from_static(value.as_bytes()),
            )]);
            let hg_cs = snapshot_hg_cs(&fixture, extra)?;
            assert_invalid(convert(&fixture, hg_cs).await, expected);
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_untracked_not_in_manifest(
        fb: FacebookInit,
    ) -> Result<()> {
        let fixture = fixture(fb).await?;
        let hg_cs = snapshot_hg_cs(&fixture, snapshot_extra(&["not_on_disk"], &[])?)?;
        assert_invalid(convert(&fixture, hg_cs).await, "not_on_disk");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_missing_in_manifest(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        let hg_cs = snapshot_hg_cs(&fixture, snapshot_extra(&[], &["unchanged"])?)?;
        assert_invalid(convert(&fixture, hg_cs).await, "unchanged");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_missing_with_change(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        let hg_cs = snapshot_hg_cs(&fixture, snapshot_extra(&[], &["modified"])?)?;
        assert_invalid(convert(&fixture, hg_cs).await, "modified");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_untracked_and_missing(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        let hg_cs = snapshot_hg_cs(&fixture, snapshot_extra(&["untracked"], &["untracked"])?)?;
        assert_invalid(
            convert(&fixture, hg_cs).await,
            "untracked is both untracked and missing",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_directory_at_missing_path(
        fb: FacebookInit,
    ) -> Result<()> {
        let fixture = fixture(fb).await?;
        // `sl add x; rm x; mkdir x; echo y > x/y` leaves `x` missing and
        // `x/y` untracked.
        let working_copy =
            CreateCommitContext::new(&fixture.ctx, &fixture.repo, vec![fixture.parent])
                .add_file("x/y", "y")
                .commit()
                .await?;
        let snapshot_manifest = load_hg_changeset(&fixture.ctx, &fixture.repo, working_copy)
            .await?
            .manifestid();
        let fixture = Fixture {
            working_copy,
            snapshot_manifest,
            ..fixture
        };
        let hg_cs = snapshot_hg_cs(&fixture, snapshot_extra(&["x/y"], &["x"])?)?;

        let bcs = convert(&fixture, hg_cs).await?;

        let working_copy = fixture
            .working_copy
            .load(&fixture.ctx, fixture.repo.repo_blobstore())
            .await?;
        let untracked = working_copy
            .file_changes_map()
            .get(&NonRootMPath::new("x/y")?)
            .and_then(FileChange::simplify)
            .ok_or_else(|| anyhow!("x/y has no contents in the working copy"))?;
        let changes = bcs.file_changes_map();
        assert_eq!(
            changes.get(&NonRootMPath::new("x")?),
            Some(&FileChange::UntrackedDeletion)
        );
        assert_eq!(
            changes.get(&NonRootMPath::new("x/y")?),
            Some(&FileChange::untracked(
                untracked.content_id(),
                untracked.file_type(),
                "y".len() as u64,
            ))
        );
        assert_eq!(changes.len(), 2);
        assert!(bcs.is_snapshot());
        Ok(())
    }

    /// A bubble and a parent commit in permanent storage, for storing
    /// snapshots.
    struct StoreFixture {
        ctx: CoreContext,
        /// The permanent repo.
        repo: Repo,
        /// Opened in the bubble.
        hg: HgRepoContext<Repo>,
        bubble_id: BubbleId,
        parent: ChangesetId,
        parent_hg: HgBlobChangeset,
        parent_root: ManifestContent,
    }

    async fn store_fixture(fb: FacebookInit) -> Result<StoreFixture> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_ephemeral_store()
            .build()
            .await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("kept/x", "x")
            .add_file("missing", "missing")
            .add_file("modified", "old")
            .add_file("removed", "removed")
            .add_file("unchanged", "unchanged")
            .commit()
            .await?;
        let parent_hg = load_hg_changeset(&ctx, &repo, parent).await?;
        let parent_root = parent_hg
            .manifestid()
            .load(&ctx, repo.repo_blobstore())
            .await?
            .content()
            .clone();
        let bubble = repo
            .repo_ephemeral_store()
            .create_bubble(&ctx, None, vec![])
            .await?;
        let bubble_id = bubble.bubble_id();
        let hg = RepoContext::new_test(ctx.clone(), Arc::new(repo.with_bubble(bubble)))
            .await?
            .hg();
        Ok(StoreFixture {
            ctx,
            repo,
            hg,
            bubble_id,
            parent,
            parent_hg,
            parent_root,
        })
    }

    type TreeEntry = Entry<HgManifestId, HgFileNodeId>;

    impl StoreFixture {
        /// The parent commit's root entry `name`.
        fn parent_entry<'a>(&self, name: &'a str) -> Result<(&'a str, TreeEntry)> {
            let entry = self
                .parent_root
                .files
                .get(&mononoke_types::MPathElement::new_from_slice(
                    name.as_bytes(),
                )?)
                .ok_or_else(|| anyhow!("{name} is not in the parent"))?;
            Ok((name, entry.clone().map_leaf(|(_, filenode)| filenode)))
        }

        /// The parent's root entries for `names`.
        fn parent_entries<'a>(&self, names: &[&'a str]) -> Result<Vec<(&'a str, TreeEntry)>> {
            names.iter().map(|name| self.parent_entry(name)).collect()
        }

        fn parent_filenode(&self, name: &str) -> Result<HgFileNodeId> {
            match self.parent_entry(name)?.1 {
                Entry::Leaf(filenode) => Ok(filenode),
                Entry::Tree(_) => Err(anyhow!("{name} is a tree in the parent")),
            }
        }

        fn upload(
            &self,
            root: &SnapshotTree,
            trees: Vec<SnapshotTree>,
            filenodes: Vec<SnapshotFilenode>,
            untracked: &[&str],
            missing: &[&str],
        ) -> Result<SnapshotUpload> {
            let changeset = HgChangesetContent::new_from_parts(
                HgParents::new(
                    Some(self.parent_hg.get_changeset_id().into_nodehash()),
                    None,
                ),
                HgManifestId::new(root.node_id),
                changeset_metadata(snapshot_extra(untracked, missing)?)?,
                vec![],
            );
            Ok(SnapshotUpload {
                hg_changeset_id: changeset.compute_hash()?,
                changeset,
                trees,
                filenodes,
            })
        }

        async fn store(
            &self,
            upload: SnapshotUpload,
        ) -> Result<StoredSnapshot, SnapshotUploadError> {
            self.hg.store_snapshot(upload).await
        }

        async fn is_present(&self, blobstore: &RepoBlobstore, key: &str) -> Result<bool> {
            Ok(blobstore
                .is_present(&self.ctx, key)
                .await?
                .assume_not_found_if_unsure())
        }

        async fn in_bubble(&self, key: &str) -> Result<bool> {
            self.is_present(self.hg.repo().repo_blobstore(), key).await
        }

        async fn in_permanent_storage(&self, key: &str) -> Result<bool> {
            self.is_present(self.repo.repo_blobstore(), key).await
        }

        /// No commit is stored in the bubble, so nothing refers to what a
        /// rejected request stored.
        async fn assert_no_commit(&self) -> Result<()> {
            assert_eq!(
                self.repo
                    .repo_ephemeral_store()
                    .changesets_from_bubble(&self.ctx, &self.bubble_id)
                    .await?,
                vec![],
                "no commit is stored in the bubble"
            );
            Ok(())
        }

        async fn assert_nothing_stored(&self) -> Result<()> {
            assert_eq!(
                self.repo
                    .repo_ephemeral_store()
                    .keys_in_bubble(&self.ctx, self.bubble_id, None, 100)
                    .await?,
                Vec::<String>::new(),
                "no blob is stored in the bubble"
            );
            self.assert_no_commit().await
        }
    }

    /// A tree with sorted `entries`, whose leaves are regular files.
    fn tree(entries: Vec<(&str, TreeEntry)>, p1: Option<HgManifestId>) -> SnapshotTree {
        let mut entries = entries;
        entries.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
        let data: String = entries
            .into_iter()
            .map(|(name, entry)| match entry {
                Entry::Tree(id) => format!("{name}\0{}t\n", id.into_nodehash()),
                Entry::Leaf(id) => format!("{name}\0{}\n", id.into_nodehash()),
            })
            .collect();
        let data = Bytes::from(data);
        let p1 = p1.map(HgManifestId::into_nodehash);
        SnapshotTree {
            node_id: calculate_hg_node_id(&data, &HgParents::new(p1, None)),
            p1,
            p2: None,
            data,
        }
    }

    fn tree_entry(tree: &SnapshotTree) -> TreeEntry {
        Entry::Tree(HgManifestId::new(tree.node_id))
    }

    fn filenode_entry(filenode: &SnapshotFilenode) -> TreeEntry {
        Entry::Leaf(filenode.node_id)
    }

    fn filenode_id(contents: &[u8], p1: Option<HgFileNodeId>) -> HgFileNodeId {
        HgFileNodeId::new(
            HgBlobNode::new(
                Bytes::copy_from_slice(contents),
                p1.map(HgFileNodeId::into_nodehash),
                None,
            )
            .nodeid(),
        )
    }

    /// A filenode without metadata whose contents are in the request.
    fn inline_filenode(contents: &'static str, p1: Option<HgFileNodeId>) -> SnapshotFilenode {
        SnapshotFilenode {
            node_id: filenode_id(contents.as_bytes(), p1),
            p1,
            p2: None,
            metadata: Bytes::new(),
            contents: SnapshotFileContents::Inline(Bytes::from_static(contents.as_bytes())),
        }
    }

    /// A filenode without metadata whose contents are named by content id.
    fn uploaded_filenode(contents: &'static str) -> SnapshotFilenode {
        let content_id =
            FileContents::content_id_for_bytes(&Bytes::from_static(contents.as_bytes()));
        SnapshotFilenode {
            node_id: filenode_id(contents.as_bytes(), None),
            p1: None,
            p2: None,
            metadata: Bytes::new(),
            contents: SnapshotFileContents::Uploaded(content_id),
        }
    }

    /// The keys of every object in the request.
    fn request_keys(upload: &SnapshotUpload) -> Vec<String> {
        let changeset = upload.hg_changeset_id.blobstore_key();
        let trees = upload
            .trees
            .iter()
            .map(|tree| HgManifestId::new(tree.node_id).blobstore_key());
        let filenodes = upload
            .filenodes
            .iter()
            .map(|filenode| filenode.node_id.blobstore_key());
        let contents = upload
            .filenodes
            .iter()
            .filter_map(|filenode| match &filenode.contents {
                SnapshotFileContents::Inline(bytes) => {
                    Some(FileContents::content_id_for_bytes(bytes).blobstore_key())
                }
                SnapshotFileContents::Uploaded(_) => None,
            });
        std::iter::once(changeset)
            .chain(trees)
            .chain(filenodes)
            .chain(contents)
            .collect()
    }

    /// The first snapshot: `modified` changed, `new/untracked` untracked,
    /// `removed` removed and `missing` missing.
    struct FirstSnapshot {
        upload: SnapshotUpload,
        modified: SnapshotFilenode,
        new: SnapshotTree,
    }

    fn first_snapshot(fixture: &StoreFixture) -> Result<FirstSnapshot> {
        let modified = inline_filenode("new", Some(fixture.parent_filenode("modified")?));
        let untracked = inline_filenode("untracked contents", None);
        let new = tree(vec![("untracked", filenode_entry(&untracked))], None);
        let mut entries = fixture.parent_entries(&["kept", "unchanged"])?;
        entries.extend([
            ("modified", filenode_entry(&modified)),
            ("new", tree_entry(&new)),
        ]);
        let root = tree(entries, Some(fixture.parent_hg.manifestid()));
        let upload = fixture.upload(
            &root,
            vec![root.clone(), new.clone()],
            vec![modified.clone(), untracked],
            &["new/untracked"],
            &["missing"],
        )?;
        Ok(FirstSnapshot {
            upload,
            modified,
            new,
        })
    }

    async fn with_snapshot_knobs(
        bubble_hg_mapping: bool,
        test: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Result<()> {
        with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                "scm/mononoke:enable_bubble_hg_mapping".to_string() => KnobVal::Bool(bubble_hg_mapping),
                "scm/mononoke:store_augmented_manifests_at_tree_upload".to_string() => KnobVal::Bool(true),
            }),
            test.boxed(),
        )
        .await
    }

    async fn assert_store_snapshot(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let first = first_snapshot(&fixture)?;

        let stored = fixture.store(first.upload.clone()).await?;

        assert_eq!(
            stored.hg_changeset.get_changeset_id(),
            first.upload.hg_changeset_id
        );
        for key in request_keys(&first.upload) {
            assert!(fixture.in_bubble(&key).await?, "{key} is in the bubble");
            assert!(
                !fixture.in_permanent_storage(&key).await?,
                "{key} is not in permanent storage"
            );
        }
        let hg_cs = stored
            .hg_changeset
            .get_changeset_id()
            .load(&fixture.ctx, fixture.hg.repo().repo_blobstore())
            .await?;
        assert_eq!(hg_cs.manifestid(), stored.hg_changeset.manifestid());
        assert_eq!(
            hg_cs.manifestid(),
            HgManifestId::new(first.upload.trees[0].node_id)
        );
        // `assert_nothing_stored` relies on the bubble listing its blobs.
        assert!(
            !fixture
                .repo
                .repo_ephemeral_store()
                .keys_in_bubble(&fixture.ctx, fixture.bubble_id, None, 100)
                .await?
                .is_empty()
        );

        let bcs_id = stored.bonsai.get_changeset_id();
        assert!(
            fixture
                .hg
                .repo()
                .commit_graph()
                .exists(&fixture.ctx, bcs_id)
                .await?
        );
        assert!(
            !fixture
                .repo
                .commit_graph()
                .exists(&fixture.ctx, bcs_id)
                .await?
        );
        assert_eq!(
            fixture
                .repo
                .repo_ephemeral_store()
                .changesets_from_bubble(&fixture.ctx, &fixture.bubble_id)
                .await?,
            vec![bcs_id]
        );
        let bcs = bcs_id
            .load(&fixture.ctx, fixture.hg.repo().repo_blobstore())
            .await?;
        assert_eq!(bcs, stored.bonsai);
        assert!(bcs.is_snapshot());
        let changes = bcs.file_changes_map();
        let change = |path: &str| -> Result<Option<&FileChange>> {
            Ok(changes.get(&NonRootMPath::new(path)?))
        };
        let new_content = FileContents::content_id_for_bytes(&Bytes::from_static(b"new"));
        assert!(
            matches!(change("modified")?, Some(FileChange::Change(tc)) if tc.content_id() == new_content)
        );
        let untracked_content =
            FileContents::content_id_for_bytes(&Bytes::from_static(b"untracked contents"));
        assert!(
            matches!(change("new/untracked")?, Some(FileChange::UntrackedChange(bc)) if bc.content_id() == untracked_content)
        );
        assert_eq!(change("removed")?, Some(&FileChange::Deletion));
        assert_eq!(change("missing")?, Some(&FileChange::UntrackedDeletion));
        assert_eq!(changes.len(), 4);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot(fb)).await
    }

    async fn assert_store_snapshot_reusing_bubble_objects(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let first = first_snapshot(&fixture)?;
        fixture.store(first.upload).await?;
        // Refers to the first snapshot's `modified` filenode and `new` tree
        // without sending them.
        let another = inline_filenode("another", None);
        let mut entries = fixture.parent_entries(&["kept", "unchanged"])?;
        entries.extend([
            ("another", filenode_entry(&another)),
            ("modified", filenode_entry(&first.modified)),
            ("new", tree_entry(&first.new)),
        ]);
        let root = tree(entries, Some(fixture.parent_hg.manifestid()));
        let upload = fixture.upload(
            &root,
            vec![root.clone()],
            vec![another],
            &["another", "new/untracked"],
            &["missing"],
        )?;

        let stored = fixture.store(upload).await?;

        let changes = stored.bonsai.file_changes_map();
        assert!(matches!(
            changes.get(&NonRootMPath::new("another")?),
            Some(FileChange::UntrackedChange(_))
        ));
        assert!(matches!(
            changes.get(&NonRootMPath::new("modified")?),
            Some(FileChange::Change(_))
        ));
        assert_eq!(changes.len(), 5);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_reusing_bubble_objects(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_reusing_bubble_objects(fb)).await
    }

    async fn assert_store_snapshot_missing_reference(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        // Neither is in the request or stored.
        let ghost_filenode = inline_filenode("ghost", None);
        let ghost_tree = tree(vec![("x", filenode_entry(&ghost_filenode))], None);
        for (name, entry, key) in [
            (
                "ghost",
                filenode_entry(&ghost_filenode),
                ghost_filenode.node_id.blobstore_key(),
            ),
            (
                "ghost_dir",
                tree_entry(&ghost_tree),
                HgManifestId::new(ghost_tree.node_id).blobstore_key(),
            ),
        ] {
            let mut entries =
                fixture.parent_entries(&["kept", "missing", "modified", "removed", "unchanged"])?;
            entries.push((name, entry));
            let root = tree(entries, Some(fixture.parent_hg.manifestid()));
            let upload = fixture.upload(&root, vec![root.clone()], vec![], &[], &[])?;

            assert_invalid(fixture.store(upload).await, &key);
        }
        fixture.assert_no_commit().await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_missing_reference(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_missing_reference(fb)).await
    }

    /// A snapshot that only adds the untracked file `uploaded`.
    fn uploaded_snapshot(
        fixture: &StoreFixture,
        uploaded: &SnapshotFilenode,
    ) -> Result<SnapshotUpload> {
        let mut entries =
            fixture.parent_entries(&["kept", "missing", "modified", "removed", "unchanged"])?;
        entries.push(("uploaded", filenode_entry(uploaded)));
        let root = tree(entries, Some(fixture.parent_hg.manifestid()));
        fixture.upload(
            &root,
            vec![root.clone()],
            vec![uploaded.clone()],
            &["uploaded"],
            &[],
        )
    }

    async fn assert_store_snapshot_uploaded_contents(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let ((content_id, _), store_contents) = filestore::store_bytes(
            fixture.hg.repo().repo_blobstore(),
            *fixture.hg.repo().filestore_config(),
            &fixture.ctx,
            Bytes::from_static(b"uploaded contents"),
        );
        store_contents.await?;
        let uploaded = uploaded_filenode("uploaded contents");
        let upload = uploaded_snapshot(&fixture, &uploaded)?;

        let stored = fixture.store(upload).await?;

        assert!(matches!(
            stored.bonsai.file_changes_map().get(&NonRootMPath::new("uploaded")?),
            Some(FileChange::UntrackedChange(bc)) if bc.content_id() == content_id
        ));
        assert!(fixture.in_bubble(&uploaded.node_id.blobstore_key()).await?);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_uploaded_contents(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_uploaded_contents(fb)).await
    }

    async fn assert_store_snapshot_missing_uploaded_contents(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let uploaded = uploaded_filenode("never uploaded");
        let upload = uploaded_snapshot(&fixture, &uploaded)?;
        let content_id = FileContents::content_id_for_bytes(&Bytes::from_static(b"never uploaded"));

        assert_invalid(
            fixture.store(upload).await,
            &format!("contents {content_id} that are not stored"),
        );
        fixture.assert_no_commit().await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_missing_uploaded_contents(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_missing_uploaded_contents(fb)).await
    }

    async fn assert_store_snapshot_wrong_filenode_hash(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let tampered = SnapshotFilenode {
            contents: SnapshotFileContents::Inline(Bytes::from_static(b"tampered")),
            ..inline_filenode("added", None)
        };
        let upload = added_snapshot(&fixture, &tampered)?;

        assert_invalid(
            fixture.store(upload).await,
            "Inconsistent node hash for entry",
        );
        // The contents may be stored before the filenode's hash is checked.
        fixture.assert_no_commit().await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_wrong_filenode_hash(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_wrong_filenode_hash(fb)).await
    }

    async fn assert_store_snapshot_wrong_changeset_id(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let upload = SnapshotUpload {
            hg_changeset_id: fixture.parent_hg.get_changeset_id(),
            ..first_snapshot(&fixture)?.upload
        };

        assert_invalid(fixture.store(upload).await, "hg changeset id");
        fixture.assert_nothing_stored().await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_wrong_changeset_id(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_wrong_changeset_id(fb)).await
    }

    async fn assert_store_snapshot_wrong_tree_hash(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let mut upload = first_snapshot(&fixture)?.upload;
        upload.trees[1].data = Bytes::from_static(b"tampered");

        assert_invalid(
            fixture.store(upload).await,
            "Inconsistent node hash for entry",
        );
        fixture.assert_nothing_stored().await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_wrong_tree_hash(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_wrong_tree_hash(fb)).await
    }

    async fn assert_store_snapshot_missing_parent(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let ghost_parent = HgChangesetId::new(calculate_hg_node_id(
            b"never stored",
            &HgParents::new(None, None),
        ));
        let first = first_snapshot(&fixture)?;
        let changeset = HgChangesetContent::new_from_parts(
            HgParents::new(Some(ghost_parent.into_nodehash()), None),
            HgManifestId::new(first.upload.trees[0].node_id),
            changeset_metadata(snapshot_extra(&[], &[])?)?,
            vec![],
        );
        let upload = SnapshotUpload {
            hg_changeset_id: changeset.compute_hash()?,
            changeset,
            ..first.upload
        };

        assert_invalid(
            fixture.store(upload).await,
            &format!("the parent commit {ghost_parent} is not stored"),
        );
        fixture.assert_nothing_stored().await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_missing_parent(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_missing_parent(fb)).await
    }

    async fn assert_store_snapshot_mapping_row(fb: FacebookInit, enabled: bool) -> Result<()> {
        let fixture = store_fixture(fb).await?;

        let stored = fixture.store(first_snapshot(&fixture)?.upload).await?;

        let bubble = fixture
            .repo
            .repo_ephemeral_store()
            .bubble_from_hg_changeset(&fixture.ctx, stored.hg_changeset.get_changeset_id())
            .await?;
        if enabled {
            assert_eq!(bubble, Some(fixture.bubble_id));
            let bonsai = fixture
                .hg
                .repo()
                .bonsai_hg_mapping()
                .get_bonsai_from_hg(&fixture.ctx, stored.hg_changeset.get_changeset_id())
                .await?;
            assert_eq!(bonsai, Some(stored.bonsai.get_changeset_id()));
        } else {
            assert_eq!(bubble, None, "no mapping row with the knob off");
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_mapping_row(fb: FacebookInit) -> Result<()> {
        for enabled in [true, false] {
            with_snapshot_knobs(enabled, assert_store_snapshot_mapping_row(fb, enabled)).await?;
        }
        Ok(())
    }

    async fn assert_store_snapshot_outside_bubble(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let upload = first_snapshot(&fixture)?.upload;
        let permanent = RepoContext::new_test(fixture.ctx.clone(), Arc::new(fixture.repo.clone()))
            .await?
            .hg();

        match permanent.store_snapshot(upload).await {
            Err(SnapshotUploadError::Internal(err)) => {
                assert!(format!("{err:#}").contains("bubble"), "{err:#}")
            }
            other => panic!("expected an internal error, got {other:?}"),
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_outside_bubble(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_outside_bubble(fb)).await
    }

    /// A snapshot that only adds `added`, whose filenode is `added_filenode`.
    fn added_snapshot(
        fixture: &StoreFixture,
        added_filenode: &SnapshotFilenode,
    ) -> Result<SnapshotUpload> {
        let mut entries =
            fixture.parent_entries(&["kept", "missing", "modified", "removed", "unchanged"])?;
        entries.push(("added", filenode_entry(added_filenode)));
        let root = tree(entries, Some(fixture.parent_hg.manifestid()));
        fixture.upload(
            &root,
            vec![root.clone()],
            vec![added_filenode.clone()],
            &[],
            &[],
        )
    }

    /// Stores in the bubble what a request that failed while writing its
    /// objects can leave there: a filenode without its contents `content_id`.
    async fn put_filenode_without_contents(
        fixture: &StoreFixture,
        content_id: ContentId,
    ) -> Result<HgFileNodeId> {
        let orphan = HgFileEnvelopeMut {
            node_id: filenode_id(b"orphan", None),
            p1: None,
            p2: None,
            content_id,
            content_size: 6,
            metadata: Bytes::new(),
        };
        fixture
            .hg
            .repo()
            .repo_blobstore()
            .put(
                &fixture.ctx,
                orphan.node_id.blobstore_key(),
                orphan.clone().freeze().into_blob().into(),
            )
            .await?;
        Ok(orphan.node_id)
    }

    async fn assert_store_snapshot_missing_contents_of_left_out_filenode(
        fb: FacebookInit,
    ) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let content_id = FileContents::content_id_for_bytes(&Bytes::from_static(b"orphan"));
        let orphan = put_filenode_without_contents(&fixture, content_id).await?;
        let mut entries =
            fixture.parent_entries(&["kept", "missing", "modified", "removed", "unchanged"])?;
        entries.push(("orphan", Entry::Leaf(orphan)));
        let root = tree(entries, Some(fixture.parent_hg.manifestid()));
        let upload = fixture.upload(&root, vec![root.clone()], vec![], &[], &[])?;

        assert_invalid(fixture.store(upload).await, &content_id.to_string());
        fixture.assert_no_commit().await
    }

    async fn assert_store_snapshot_inline_contents_with_metadata_header(
        fb: FacebookInit,
    ) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        // Without separate metadata, hg reads a metadata header at the start
        // of the contents, so only `stripped` is stored.
        let raw: &'static [u8] = b"\x01\n\x01\nstripped";
        let header = SnapshotFilenode {
            node_id: filenode_id(raw, None),
            p1: None,
            p2: None,
            metadata: Bytes::new(),
            contents: SnapshotFileContents::Inline(Bytes::from_static(raw)),
        };
        let raw_content_id = FileContents::content_id_for_bytes(&Bytes::from_static(raw));
        let orphan = put_filenode_without_contents(&fixture, raw_content_id).await?;
        let mut entries =
            fixture.parent_entries(&["kept", "missing", "modified", "removed", "unchanged"])?;
        entries.extend([
            ("header", filenode_entry(&header)),
            ("orphan", Entry::Leaf(orphan)),
        ]);
        let root = tree(entries, Some(fixture.parent_hg.manifestid()));
        let upload = fixture.upload(&root, vec![root.clone()], vec![header], &[], &[])?;

        assert_invalid(fixture.store(upload).await, &raw_content_id.to_string());
        fixture.assert_no_commit().await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_inline_contents_with_metadata_header(
        fb: FacebookInit,
    ) -> Result<()> {
        with_snapshot_knobs(
            true,
            assert_store_snapshot_inline_contents_with_metadata_header(fb),
        )
        .await
    }

    async fn assert_store_snapshot_same_bonsai_as_stored_snapshot(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let first = first_snapshot(&fixture)?.upload;
        let stored = fixture.store(first.clone()).await?;
        // The bonsai does not record the hg commit's list of changed files.
        let changeset = HgChangesetContent::new_from_parts(
            HgParents::new(
                Some(fixture.parent_hg.get_changeset_id().into_nodehash()),
                None,
            ),
            HgManifestId::new(first.trees[0].node_id),
            changeset_metadata(snapshot_extra(&["new/untracked"], &["missing"])?)?,
            vec![NonRootMPath::new("modified")?],
        );
        let second = SnapshotUpload {
            hg_changeset_id: changeset.compute_hash()?,
            changeset,
            ..first.clone()
        };
        assert_ne!(second.hg_changeset_id, first.hg_changeset_id);

        assert_invalid(
            fixture.store(second).await,
            &format!("already stored as hg commit {}", first.hg_changeset_id),
        );
        assert_eq!(
            fixture
                .hg
                .repo()
                .bonsai_hg_mapping()
                .get_bonsai_from_hg(&fixture.ctx, first.hg_changeset_id)
                .await?,
            Some(stored.bonsai.get_changeset_id())
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_same_bonsai_as_stored_snapshot(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(
            true,
            assert_store_snapshot_same_bonsai_as_stored_snapshot(fb),
        )
        .await
    }

    async fn assert_store_snapshot_in_another_bubble(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        let upload = first_snapshot(&fixture)?.upload;
        fixture.store(upload.clone()).await?;
        let other_bubble = fixture
            .repo
            .repo_ephemeral_store()
            .create_bubble(&fixture.ctx, None, vec![])
            .await?;
        let other = RepoContext::new_test(
            fixture.ctx.clone(),
            Arc::new(fixture.repo.with_bubble(other_bubble)),
        )
        .await?
        .hg();

        match other.store_snapshot(upload).await {
            Err(SnapshotUploadError::InAnotherBubble(bubble_id)) => {
                assert_eq!(bubble_id, fixture.bubble_id)
            }
            other => panic!("expected the snapshot to be in another bubble, got {other:?}"),
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_in_another_bubble(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_in_another_bubble(fb)).await
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_missing_contents_of_left_out_filenode(
        fb: FacebookInit,
    ) -> Result<()> {
        with_snapshot_knobs(
            true,
            assert_store_snapshot_missing_contents_of_left_out_filenode(fb),
        )
        .await
    }

    async fn assert_store_snapshot_builds_augmented_manifests(fb: FacebookInit) -> Result<()> {
        let fixture = store_fixture(fb).await?;
        fixture
            .repo
            .repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(
                &fixture.ctx,
                fixture.parent,
                DerivationPriority::LOW,
            )
            .await?;
        let first = first_snapshot(&fixture)?;

        fixture.store(first.upload.clone()).await?;

        for tree in &first.upload.trees {
            let key = HgAugmentedManifestId::new(tree.node_id).blobstore_key();
            assert!(fixture.in_bubble(&key).await?, "{key} is in the bubble");
            assert!(
                !fixture.in_permanent_storage(&key).await?,
                "{key} is not in permanent storage"
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_store_snapshot_builds_augmented_manifests(fb: FacebookInit) -> Result<()> {
        with_snapshot_knobs(true, assert_store_snapshot_builds_augmented_manifests(fb)).await
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_non_utf8_extra_key(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        let mut extra = snapshot_extra(&[], &[])?;
        extra.insert(Bytes::from_static(b"bad\xff"), Bytes::from_static(b"value"));
        let hg_cs = snapshot_hg_cs(&fixture, extra)?;
        assert_invalid(
            convert(&fixture, hg_cs).await,
            r#"the extra key b"bad\xff" is not valid UTF-8"#,
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_snapshot_bonsai_from_hg_author_with_newline(fb: FacebookInit) -> Result<()> {
        let fixture = fixture(fb).await?;
        let metadata = ChangesetMetadata {
            user: "Alice\n<alice@example.com>".to_string(),
            ..changeset_metadata(snapshot_extra(&[], &[])?)?
        };
        let hg_cs = snapshot_hg_cs_with_metadata(&fixture, metadata)?;
        assert_invalid(
            convert(&fixture, hg_cs).await,
            "the author contains a newline",
        );
        Ok(())
    }
}
