/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Conversion of snapshots uploaded as hg commits into snapshot bonsais.

use std::collections::HashSet;

use anyhow::anyhow;
use blobrepo_hg::create_bonsai_changeset_object;
use context::CoreContext;
use futures::TryStreamExt;
use futures::future;
use manifest::Entry;
use manifest::ManifestOps;
use mercurial_types::HgManifestId;
use mercurial_types::blobs::HgBlobChangeset;
use mercurial_types::blobs::snapshot_state;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::FileChange;
use mononoke_types::NonRootMPath;
use repo_blobstore::RepoBlobstore;
use thiserror::Error;

/// An error from turning an uploaded snapshot into Mononoke objects.
#[derive(Debug, Error)]
pub enum SnapshotUploadError {
    /// The client sent something that is not a valid snapshot.
    #[error("Invalid snapshot: {0:#}")]
    InvalidSnapshot(anyhow::Error),
    /// A storage or other server failure. Conversion failures that the checks
    /// for an invalid snapshot do not catch are reported here too, for example
    /// copy information whose source is in no parent.
    #[error(transparent)]
    Internal(anyhow::Error),
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

    use anyhow::Result;
    use blobstore::Loadable;
    use bytes::Bytes;
    use fbinit::FacebookInit;
    use mercurial_derivation::DeriveHgChangeset;
    use mercurial_types::HgParents;
    use mercurial_types::blobs::ChangesetMetadata;
    use mercurial_types::blobs::HgChangesetContent;
    use mercurial_types::blobs::SNAPSHOT_METADATA_KEY;
    use mononoke_api::repo::Repo;
    use mononoke_macros::mononoke;
    use mononoke_types::DateTime;
    use repo_blobstore::RepoBlobstoreRef;
    use tests_utils::CreateCommitContext;

    use super::*;

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

    fn assert_invalid(result: Result<BonsaiChangeset, SnapshotUploadError>, expected: &str) {
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
