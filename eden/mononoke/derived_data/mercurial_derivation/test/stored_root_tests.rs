/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Augmented derivation of a commit with a mapped Hg changeset maps it to the
//! augmented manifest stored under its root tree id whenever one is stored,
//! including one that fails a check. It builds from the Hg manifest only to
//! store directory records the root lacks, or when no record is stored.

use std::collections::HashMap;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use blobstore::BlobstoreBytes;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bytes::Bytes;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::BonsaiDerivable;
use fbinit::FacebookInit;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use manifest::Entry;
use manifest::ManifestOps;
use memblob::Memblob;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::derive_hg_augmented_manifest;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::ShardedHgAugmentedManifest;
use metaconfig_types::PathRestrictionMetadata;
use metaconfig_types::RestrictedPathsConfig;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use mononoke_types::hash::Blake2;
use mononoke_types::sha1_hash::Sha1;
use mononoke_types::typed_hash::AclManifestId;
use permission_checker::MononokeIdentity;
use permission_checker::dummy::DummyAclProvider;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths::ManifestType;
use restricted_paths::RestrictedManifestId;
use restricted_paths::RestrictedPathManifestIdEntry;
use restricted_paths::RestrictedPaths;
use restricted_paths::RestrictedPathsConfigBased;
use restricted_paths::RestrictedPathsManifestIdStore;
use restricted_paths::RestrictedPathsRef;
use scuba_ext::MononokeScubaSampleBuilder;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;

use crate::Repo;
use crate::tracing_blobstore::TracingBlobstore;

/// The two entry points of augmented derivation: the derived data manager
/// derives through `derive_batch`, and `derive_single` is called directly.
#[derive(Clone, Copy, Debug)]
enum Derive {
    Single,
    Batch,
}

const DERIVE_MODES: [Derive; 2] = [Derive::Single, Derive::Batch];

impl Derive {
    /// Derive `cs_id`'s augmented manifest. Its parents' must be derived.
    async fn root(
        self,
        ctx: &CoreContext,
        repo: &Repo,
        cs_id: ChangesetId,
    ) -> Result<HgAugmentedManifestId> {
        // Neither entry point derives dependencies.
        repo.repo_derived_data()
            .derive::<RootAclManifestId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
        let manager = repo.repo_derived_data().manager();
        let root = match self {
            Derive::Single => {
                let bonsai = cs_id.load(ctx, repo.repo_blobstore()).await?;
                let mut parents = Vec::new();
                for parent in bonsai.parents() {
                    parents.push(
                        manager
                            .fetch_derived::<RootHgAugmentedManifestId>(ctx, parent, None)
                            .await?
                            .with_context(|| format!("missing augmented manifest for {parent}"))?,
                    );
                }
                RootHgAugmentedManifestId::derive_single(
                    ctx,
                    &manager.derivation_context(None),
                    bonsai,
                    parents,
                    None,
                )
                .await?
            }
            Derive::Batch => {
                manager
                    .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, vec![cs_id], None)
                    .await?;
                manager
                    .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
                    .await?
                    .with_context(|| format!("missing augmented manifest for {cs_id}"))?
            }
        };
        Ok(root.hg_augmented_manifest_id())
    }
}

/// A child commit stored the way a client upload stores it: it and its parent
/// have Hg changesets and ACL manifests, and only the parent has an augmented
/// manifest.
struct MappedCommit {
    parent_root: HgAugmentedManifestId,
    child: ChangesetId,
    hg_root: HgManifestId,
}

impl MappedCommit {
    async fn new(ctx: &CoreContext, repo: &Repo) -> Result<Self> {
        Self::with_files(ctx, repo, &[("dir/a", "a")], &[("dir/b", "b"), ("c", "c")]).await
    }

    async fn with_files(
        ctx: &CoreContext,
        repo: &Repo,
        parent_files: &[(&str, &str)],
        child_files: &[(&str, &str)],
    ) -> Result<Self> {
        let parent = parent_files
            .iter()
            .fold(
                CreateCommitContext::new_root(ctx, repo),
                |commit, (path, content)| commit.add_file(*path, *content),
            )
            .commit()
            .await?;
        let child = child_files
            .iter()
            .fold(
                CreateCommitContext::new(ctx, repo, vec![parent]),
                |commit, (path, content)| commit.add_file(*path, *content),
            )
            .commit()
            .await?;
        fixtures::store_hg_changesets_as_uploaded(ctx, repo, vec![parent, child]).await?;
        let manager = repo.repo_derived_data().manager();
        manager
            .derive_exactly_batch::<RootAclManifestId>(ctx, vec![parent, child], None)
            .await?;
        manager
            .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, vec![parent], None)
            .await?;
        let parent_root = manager
            .fetch_derived::<RootHgAugmentedManifestId>(ctx, parent, None)
            .await?
            .context("missing parent augmented manifest")?
            .hg_augmented_manifest_id();
        let hg_root = repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(ctx, child)
            .await?
            .context("missing child Hg changeset")?
            .load(ctx, repo.repo_blobstore())
            .await?
            .manifestid();
        Ok(Self {
            parent_root,
            child,
            hg_root,
        })
    }

    fn root_id(&self) -> HgAugmentedManifestId {
        HgAugmentedManifestId::from(self.hg_root)
    }

    fn hg_root_key(&self) -> String {
        self.hg_root.blobstore_key()
    }

    fn parent_hg_root_key(&self) -> String {
        HgManifestId::new(self.parent_root.into_nodehash()).blobstore_key()
    }

    /// Store the child's augmented manifest under its root tree id without
    /// mapping the commit to it, as tree upload does.
    async fn store_root(&self, ctx: &CoreContext, repo: &Repo) -> Result<()> {
        let root = derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
            ctx,
            repo.repo_blobstore(),
            self.hg_root,
            vec![self.parent_root],
            &Default::default(),
            repo.restricted_paths().config_based(),
            None,
        )
        .await?;
        assert_eq!(root, self.root_id(), "the stored root is the child's tree");
        Ok(())
    }

    /// Replace the stored root record with a changed copy under the same key,
    /// and return the changed record. Puts do not overwrite, so the record is
    /// removed first.
    async fn tamper_root(
        &self,
        ctx: &CoreContext,
        repo: &Repo,
        tamper: impl FnOnce(&mut ShardedHgAugmentedManifest),
    ) -> Result<ShardedHgAugmentedManifest> {
        let mut envelope = self.root_id().load(ctx, repo.repo_blobstore()).await?;
        tamper(&mut envelope.augmented_manifest);
        let changed = envelope.augmented_manifest.clone();
        repo.repo_blobstore()
            .unlink(ctx, &self.root_id().blobstore_key())
            .await?;
        let key = envelope.store(ctx, repo.repo_blobstore()).await?;
        assert_eq!(key, self.root_id(), "the changed record keeps its key");
        assert_eq!(
            self.stored_root(ctx, repo).await?,
            changed,
            "the changed record replaced the stored one"
        );
        Ok(changed)
    }

    /// The record stored under the child's root tree id.
    async fn stored_root(
        &self,
        ctx: &CoreContext,
        repo: &Repo,
    ) -> Result<ShardedHgAugmentedManifest> {
        Ok(self
            .root_id()
            .load(ctx, repo.repo_blobstore())
            .await?
            .augmented_manifest)
    }

    /// Remove blobs, as Hg manifests are once they are no longer written, and
    /// return them so they can be put back.
    async fn remove(
        &self,
        ctx: &CoreContext,
        repo: &Repo,
        keys: Vec<String>,
    ) -> Result<Vec<(String, BlobstoreBytes)>> {
        let mut removed = Vec::new();
        for key in keys {
            let bytes = repo
                .repo_blobstore()
                .get(ctx, &key)
                .await?
                .with_context(|| format!("missing {key}"))?
                .into_bytes();
            repo.repo_blobstore().unlink(ctx, &key).await?;
            removed.push((key, bytes));
        }
        Ok(removed)
    }

    /// Derive the child with the blobs under `keys` removed, then put them
    /// back. Also returns the Hg manifests derivation read.
    async fn derive_without(
        &self,
        ctx: &CoreContext,
        repo: &Repo,
        hg_reads: &TracingBlobstore<Memblob>,
        derive: Derive,
        keys: Vec<String>,
    ) -> Result<(Result<HgAugmentedManifestId>, Vec<String>)> {
        let removed = self.remove(ctx, repo, keys).await?;
        hg_manifest_reads(hg_reads);
        let result = derive.root(ctx, repo, self.child).await;
        let read = hg_manifest_reads(hg_reads);
        for (key, bytes) in removed {
            repo.repo_blobstore().put(ctx, key, bytes).await?;
        }
        Ok((result, read))
    }

    /// Derive the child with or without its root Hg manifest.
    async fn derive(
        &self,
        ctx: &CoreContext,
        repo: &Repo,
        hg_reads: &TracingBlobstore<Memblob>,
        derive: Derive,
        with_hg: bool,
    ) -> Result<(Result<HgAugmentedManifestId>, Vec<String>)> {
        let removed = if with_hg {
            vec![]
        } else {
            vec![self.hg_root_key()]
        };
        self.derive_without(ctx, repo, hg_reads, derive, removed)
            .await
    }

    /// The child's `dir`, which it changes, so its record is new over the
    /// parent's.
    async fn changed_dir(&self, ctx: &CoreContext, repo: &Repo) -> Result<HgAugmentedManifestId> {
        self.root_id()
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore().clone(),
                MPath::new("dir")?,
            )
            .await?
            .and_then(Entry::into_tree)
            .context("the child's root has dir")
    }
}

/// The Hg manifest blobs read beneath the repo blobstore since the last call.
fn hg_manifest_reads(tracing: &TracingBlobstore<Memblob>) -> Vec<String> {
    tracing
        .tracing_gets()
        .into_iter()
        .filter(|key| key.contains("hgmanifest."))
        .collect()
}

/// A repo that records its reads, so tests can tell a stored root that is
/// mapped directly from one derivation builds from Hg.
async fn repo_recording_hg_reads(
    fb: FacebookInit,
) -> Result<(Repo, Arc<TracingBlobstore<Memblob>>)> {
    let tracing = Arc::new(TracingBlobstore::new(Memblob::default()));
    let repo = TestRepoFactory::new(fb)?
        .with_blobstore(tracing.clone())
        .build()
        .await?;
    Ok((repo, tracing))
}

/// Both entry points, with and without the commit's root Hg manifest.
fn cases() -> impl Iterator<Item = (Derive, bool)> {
    DERIVE_MODES
        .into_iter()
        .flat_map(|derive| [(derive, true), (derive, false)])
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_uses_stored_root(fb: FacebookInit) -> Result<()> {
    for derive in DERIVE_MODES {
        // Given: a usable stored root, and the commit's Hg manifests.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        hg_manifest_reads(&hg_reads);

        // When: deriving the commit.
        let root = derive.root(&ctx, &repo, commit.child).await?;

        // Then: it maps to the stored root without reading an Hg manifest.
        assert_eq!(root, commit.root_id(), "{derive:?}");
        assert_eq!(
            hg_manifest_reads(&hg_reads),
            Vec::<String>::new(),
            "{derive:?}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_stored_root_with_another_hg_id(fb: FacebookInit) -> Result<()> {
    for (derive, with_hg) in cases() {
        // Given: a stored root whose entries hash to another id than the one
        // it is stored under, because its p1 was dropped.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        let tampered = commit
            .tamper_root(&ctx, &repo, |manifest| {
                assert!(manifest.p1.is_some(), "the child's root has a parent");
                manifest.p1 = None;
            })
            .await?;

        // When: deriving the commit.
        let (root, read) = commit
            .derive(&ctx, &repo, &hg_reads, derive, with_hg)
            .await?;
        let root = root?;

        // Then: the commit maps to the rejected record directly, without
        // reading an Hg manifest.
        assert_eq!(root, commit.root_id(), "{derive:?}, Hg {with_hg}");
        assert_eq!(
            commit.stored_root(&ctx, &repo).await?,
            tampered,
            "{derive:?}, Hg {with_hg}"
        );
        assert_eq!(read, Vec::<String>::new(), "{derive:?}, Hg {with_hg}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_stored_root_with_another_acl_manifest(fb: FacebookInit) -> Result<()> {
    for (derive, with_hg) in cases() {
        // Given: a stored root that points to another ACL manifest than the
        // commit's, which is `None` because the commit has no ACL files.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        let tampered = commit
            .tamper_root(&ctx, &repo, |manifest| {
                assert_eq!(manifest.acl_manifest_directory_id, None);
                manifest.acl_manifest_directory_id = Some(other_acl());
            })
            .await?;

        // When: deriving the commit.
        let (root, read) = commit
            .derive(&ctx, &repo, &hg_reads, derive, with_hg)
            .await?;
        let root = root?;

        // Then: the commit maps to the rejected record directly, without
        // reading an Hg manifest.
        assert_eq!(root, commit.root_id(), "{derive:?}, Hg {with_hg}");
        assert_eq!(
            commit.stored_root(&ctx, &repo).await?,
            tampered,
            "{derive:?}, Hg {with_hg}"
        );
        assert_eq!(read, Vec::<String>::new(), "{derive:?}, Hg {with_hg}");
    }
    Ok(())
}

fn other_acl() -> AclManifestId {
    AclManifestId::new(Blake2::from_byte_array([1; 32]))
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_stored_root_missing_a_new_directory(fb: FacebookInit) -> Result<()> {
    for derive in DERIVE_MODES {
        for (case, removed_hg_roots) in [
            ("no Hg root missing", vec![]),
            ("the commit's Hg root missing", vec!["child"]),
            ("the parent's Hg root missing", vec!["parent"]),
            ("both Hg roots missing", vec!["child", "parent"]),
        ] {
            // Given: a stored root whose record of `dir`, which the commit
            // changes, was never stored, as a failed tree upload can leave it.
            let ctx = CoreContext::test_mock(fb);
            let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
            let commit = MappedCommit::new(&ctx, &repo).await?;
            commit.store_root(&ctx, &repo).await?;
            let dir = commit.changed_dir(&ctx, &repo).await?;
            repo.repo_blobstore()
                .unlink(&ctx, &dir.blobstore_key())
                .await?;
            let removed = removed_hg_roots
                .iter()
                .map(|which| match *which {
                    "child" => commit.hg_root_key(),
                    _ => commit.parent_hg_root_key(),
                })
                .collect();

            // When: deriving the commit.
            let (root, read) = commit
                .derive_without(&ctx, &repo, &hg_reads, derive, removed)
                .await?;
            let root = root?;

            // Then: derivation builds from Hg, which stores the missing record
            // when every Hg manifest it needs exists. Otherwise the incomplete
            // record wins as it is.
            assert_eq!(root, commit.root_id(), "{derive:?}, {case}");
            assert!(!read.is_empty(), "{derive:?}, {case}: builds from Hg");
            let dir_stored = dir.load(&ctx, repo.repo_blobstore()).await.is_ok();
            assert_eq!(
                dir_stored,
                removed_hg_roots.is_empty(),
                "{derive:?}, {case}: dir's record should be stored exactly when the Hg build succeeded"
            );
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_rejected_stored_root_missing_a_new_directory(fb: FacebookInit) -> Result<()> {
    for (derive, with_hg) in cases() {
        // Given: a stored root that points to another ACL manifest and lacks
        // the record of `dir`.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        let tampered = commit
            .tamper_root(&ctx, &repo, |manifest| {
                manifest.acl_manifest_directory_id = Some(other_acl());
            })
            .await?;
        let dir = commit.changed_dir(&ctx, &repo).await?;
        repo.repo_blobstore()
            .unlink(&ctx, &dir.blobstore_key())
            .await?;

        // When: deriving the commit.
        let (root, read) = commit
            .derive(&ctx, &repo, &hg_reads, derive, with_hg)
            .await?;
        let root = root?;

        // Then: the rejected record stays mapped as it is, without building
        // from Hg, which would put another root under its key.
        assert_eq!(root, commit.root_id(), "{derive:?}, Hg {with_hg}");
        assert_eq!(read, Vec::<String>::new(), "{derive:?}, Hg {with_hg}");
        assert_eq!(
            commit.stored_root(&ctx, &repo).await?,
            tampered,
            "{derive:?}, Hg {with_hg}"
        );
        let dir_stored = dir.load(&ctx, repo.repo_blobstore()).await.is_ok();
        assert!(!dir_stored, "{derive:?}, Hg {with_hg}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_read_error_in_a_new_directory_is_repaired(fb: FacebookInit) -> Result<()> {
    for derive in DERIVE_MODES {
        // Given: a stored root whose new `dir` record fails to read once, and
        // which lacks the record of `dir/sub` below it.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::with_files(
            &ctx,
            &repo,
            &[("dir/a", "a")],
            &[("dir/sub/b", "b"), ("c", "c")],
        )
        .await?;
        commit.store_root(&ctx, &repo).await?;
        let dir = commit.changed_dir(&ctx, &repo).await?;
        let sub = commit
            .root_id()
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore().clone(),
                MPath::new("dir/sub")?,
            )
            .await?
            .and_then(Entry::into_tree)
            .context("the child's root has dir/sub")?;
        repo.repo_blobstore()
            .unlink(&ctx, &sub.blobstore_key())
            .await?;
        hg_reads.fail_next_get(dir.blobstore_key());

        // When: deriving the commit.
        let (root, _) = commit.derive(&ctx, &repo, &hg_reads, derive, true).await?;

        // Then: it maps the stored root after a repair, which stores the
        // record the read error hid.
        assert_eq!(root?, commit.root_id(), "{derive:?}");
        assert!(
            sub.load(&ctx, repo.repo_blobstore()).await.is_ok(),
            "{derive:?}: dir/sub's record should be stored"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_without_stored_root(fb: FacebookInit) -> Result<()> {
    for derive in DERIVE_MODES {
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::new(&ctx, &repo).await?;

        // Without its Hg manifest, a commit with no stored root fails with an
        // error saying why.
        let (result, _) = commit.derive(&ctx, &repo, &hg_reads, derive, false).await?;
        let err = result.expect_err("derivation needs the Hg manifest when no root is stored");
        let message = format!("{err:#}");
        assert!(
            message.contains("building from Hg failed")
                && message.contains("no augmented manifest is stored"),
            "{derive:?}: the error should say there is neither, got: {message}",
        );

        // With it, the commit builds from Hg and stores the record.
        let root = derive.root(&ctx, &repo, commit.child).await?;
        assert_eq!(root, commit.root_id(), "{derive:?}");
        let stored = commit.stored_root(&ctx, &repo).await?;
        assert_eq!(
            (stored.p1, stored.acl_manifest_directory_id),
            (Some(commit.parent_root.into_nodehash()), None),
            "{derive:?}: the record built from Hg is stored under the root tree id"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_empty_commit_on_root_under_client_id(fb: FacebookInit) -> Result<()> {
    for (derive, with_hg) in cases() {
        // Given: a commit whose Hg root is stored under a client-chosen id,
        // as a mirror upload stores it, with its augmented manifest derived,
        // and an empty child with an uploaded Hg changeset, so the child's
        // root tree is that id.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;
        let supplied = HgNodeHash::new(Sha1::from_byte_array([0xAB; 20]));
        fixtures::store_supplied_hg_root(&ctx, &repo, parent, supplied).await?;
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&ctx, parent, DerivationPriority::LOW)
            .await?;
        let stored = HgAugmentedManifestId::new(supplied)
            .load(&ctx, repo.repo_blobstore())
            .await?
            .augmented_manifest;
        assert_eq!(
            stored.hg_node_id, supplied,
            "fixture: stored under the client's id"
        );
        assert_ne!(
            stored.computed_node_id, supplied,
            "fixture: the client's id is not the content hash"
        );
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .commit()
            .await?;
        fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, vec![child]).await?;
        let commit = MappedCommit {
            parent_root: HgAugmentedManifestId::new(supplied),
            child,
            hg_root: HgManifestId::new(supplied),
        };

        // When: deriving the child.
        let (root, read) = commit
            .derive(&ctx, &repo, &hg_reads, derive, with_hg)
            .await?;
        let root = root?;

        // Then: it maps to the client's root directly, without reading an Hg
        // manifest, and the record stays as stored.
        assert_eq!(root, commit.root_id(), "{derive:?}, Hg {with_hg}");
        assert_eq!(read, Vec::<String>::new(), "{derive:?}, Hg {with_hg}");
        assert_eq!(
            commit.stored_root(&ctx, &repo).await?,
            stored,
            "{derive:?}, Hg {with_hg}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_stored_root_with_a_corrupt_directory(fb: FacebookInit) -> Result<()> {
    for (derive, with_hg) in cases() {
        // Given: a stored root whose record of `dir` does not decode.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        let dir_key = commit.changed_dir(&ctx, &repo).await?.blobstore_key();
        repo.repo_blobstore().unlink(&ctx, &dir_key).await?;
        repo.repo_blobstore()
            .put(&ctx, dir_key.clone(), BlobstoreBytes::from_bytes("garbage"))
            .await?;

        // When: deriving the commit.
        let (root, read) = commit
            .derive(&ctx, &repo, &hg_reads, derive, with_hg)
            .await?;

        // Then: the commit maps to the stored root without building from
        // Hg, and the corrupt record stays.
        assert_eq!(root?, commit.root_id(), "{derive:?}, Hg {with_hg}");
        assert_eq!(read, Vec::<String>::new(), "{derive:?}, Hg {with_hg}");
        let dir_bytes = repo
            .repo_blobstore()
            .get(&ctx, &dir_key)
            .await?
            .context("dir's record")?;
        assert_eq!(
            dir_bytes.into_raw_bytes(),
            Bytes::from_static(b"garbage"),
            "{derive:?}, Hg {with_hg}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_whose_parent_lacks_a_directory(fb: FacebookInit) -> Result<()> {
    for derive in DERIVE_MODES {
        // Given: a usable stored root whose parent's record of `dir`, which
        // the commit changes, is missing, and every Hg manifest.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        let parent_dir = commit
            .parent_root
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore().clone(),
                MPath::new("dir")?,
            )
            .await?
            .and_then(Entry::into_tree)
            .context("the parent's root has dir")?;
        repo.repo_blobstore()
            .unlink(&ctx, &parent_dir.blobstore_key())
            .await?;

        // When: deriving the commit.
        let (root, read) = commit.derive(&ctx, &repo, &hg_reads, derive, true).await?;

        // Then: the build from Hg cannot restore the parent's record, and the
        // commit maps to its stored root anyway.
        assert_eq!(root?, commit.root_id(), "{derive:?}");
        assert!(!read.is_empty(), "{derive:?}: tries to build from Hg");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_commit_with_root_under_client_id(fb: FacebookInit) -> Result<()> {
    for (derive, with_hg) in cases() {
        // Given: a commit whose Hg root is stored under a client-chosen id, as
        // a mirror upload stores it, with its augmented manifest stored under
        // that id as tree upload stores it, on a parent with another root.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_recording_hg_reads(fb).await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("dir/b", "b")
            .commit()
            .await?;
        let supplied = HgNodeHash::new(Sha1::from_byte_array([0xCD; 20]));
        fixtures::store_supplied_hg_root(&ctx, &repo, child, supplied).await?;
        let parent_root = repo
            .repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&ctx, parent, DerivationPriority::LOW)
            .await?
            .hg_augmented_manifest_id();
        let commit = MappedCommit {
            parent_root,
            child,
            hg_root: HgManifestId::new(supplied),
        };
        commit.store_root(&ctx, &repo).await?;
        let stored = commit.stored_root(&ctx, &repo).await?;
        assert_ne!(
            stored.computed_node_id, supplied,
            "fixture: the client's id is not the content hash"
        );

        // When: deriving the child.
        let (root, read) = commit
            .derive(&ctx, &repo, &hg_reads, derive, with_hg)
            .await?;

        // Then: it maps to the client's root directly, without reading an Hg
        // manifest.
        assert_eq!(root?, commit.root_id(), "{derive:?}, Hg {with_hg}");
        assert_eq!(read, Vec::<String>::new(), "{derive:?}, Hg {with_hg}");
        assert_eq!(
            commit.stored_root(&ctx, &repo).await?,
            stored,
            "{derive:?}, Hg {with_hg}"
        );
    }
    Ok(())
}

fn set_restricted_paths_logging(on: bool) {
    set_restricted_paths_knobs(on, false);
}

/// Sets restricted-path recording, and whether a failed recording fails
/// derivation.
fn set_restricted_paths_knobs(logging: bool, fail_derivation: bool) {
    override_just_knobs(JustKnobsInMemory::new(HashMap::from([
        (
            "scm/mononoke:enabled_restricted_paths_access_logging".to_string(),
            KnobVal::Bool(logging),
        ),
        (
            "scm/mononoke:fail_derivation_on_restricted_manifest_id_store_error".to_string(),
            KnobVal::Bool(fail_derivation),
        ),
    ])));
}

fn restricted_paths_config(path: &str) -> Result<RestrictedPathsConfig> {
    let metadata = PathRestrictionMetadata {
        repo_region_acl: MononokeIdentity::from_legacy_type_data("REPO_REGION", "test_acl"),
        permission_request_group: None,
        rollout_allowlist_group: None,
        read_only: false,
    };
    Ok(RestrictedPathsConfig {
        path_restriction_metadata: [(NonRootMPath::new(path)?, metadata)].into_iter().collect(),
        ..Default::default()
    })
}

/// A repo where `path` is a restricted root that is not read-only, so
/// derivation records its augmented manifest ids, and which records its reads.
async fn repo_restricting(
    fb: FacebookInit,
    path: &str,
) -> Result<(Repo, Arc<TracingBlobstore<Memblob>>)> {
    let config = restricted_paths_config(path)?;
    let tracing = Arc::new(TracingBlobstore::new(Memblob::default()));
    let repo = TestRepoFactory::new(fb)?
        .with_blobstore(tracing.clone())
        .with_config_override(move |cfg| cfg.restricted_paths_config = config)
        .build()
        .await?;
    Ok((repo, tracing))
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_stored_root_records_restricted_directories(
    fb: FacebookInit,
) -> Result<()> {
    for derive in DERIVE_MODES {
        // Given: a stored root, stored without recording its restricted
        // directories, for a commit whose `dir` is restricted.
        let ctx = CoreContext::test_mock(fb);
        let (repo, _) = repo_restricting(fb, "dir").await?;
        set_restricted_paths_logging(false);
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        commit
            .remove(&ctx, &repo, vec![commit.hg_root_key()])
            .await?;
        set_restricted_paths_logging(true);

        // When: deriving the commit, which uses the stored root.
        derive.root(&ctx, &repo, commit.child).await?;

        // Then: the stored root's `dir` is recorded, as mapping at upload
        // records it.
        let dir = commit
            .root_id()
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore().clone(),
                MPath::new("dir")?,
            )
            .await?
            .and_then(Entry::into_tree)
            .context("the child's root has dir")?;
        let recorded: Vec<_> = repo
            .restricted_paths()
            .config_based()
            .manifest_id_store()
            .get_all_entries(&ctx)
            .await?
            .into_iter()
            .filter(|entry| entry.manifest_type == ManifestType::HgAugmented)
            .collect();
        assert_eq!(
            recorded,
            vec![RestrictedPathManifestIdEntry::new(
                ManifestType::HgAugmented,
                dir.to_string().into(),
                RepoPath::dir("dir")?,
            )?],
            "{derive:?}: only the stored root's dir is recorded"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_whose_shared_restricted_directory_is_damaged(
    fb: FacebookInit,
) -> Result<()> {
    for derive in DERIVE_MODES {
        // Given: a usable stored root whose restricted `shared`, which the
        // commit shares unchanged with its parent, has lost its record, and
        // no Hg manifest.
        let ctx = CoreContext::test_mock(fb);
        let (repo, hg_reads) = repo_restricting(fb, "shared").await?;
        set_restricted_paths_logging(false);
        let commit = MappedCommit::with_files(
            &ctx,
            &repo,
            &[("dir/a", "a"), ("shared/s", "s")],
            &[("dir/b", "b")],
        )
        .await?;
        commit.store_root(&ctx, &repo).await?;
        let shared = commit
            .root_id()
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore().clone(),
                MPath::new("shared")?,
            )
            .await?
            .and_then(Entry::into_tree)
            .context("the child's root has shared")?;
        repo.repo_blobstore()
            .unlink(&ctx, &shared.blobstore_key())
            .await?;
        set_restricted_paths_knobs(true, true);

        // When: deriving the commit, whose restricted directories cannot all
        // be recorded, even with failed recordings failing derivation.
        let (root, read) = commit.derive(&ctx, &repo, &hg_reads, derive, false).await?;

        // Then: it maps to the stored root anyway, accepted without reading
        // an Hg manifest.
        assert_eq!(root?, commit.root_id(), "{derive:?}");
        assert_eq!(read, Vec::<String>::new(), "{derive:?}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_read_error_in_a_restricted_directory(fb: FacebookInit) -> Result<()> {
    for (derive, fail_derivation) in DERIVE_MODES
        .into_iter()
        .flat_map(|derive| [(derive, false), (derive, true)])
    {
        // Given: a usable stored root whose restricted `shared`, which the
        // commit shares unchanged with its parent, fails to read once.
        let ctx = CoreContext::test_mock(fb);
        let (repo, tracing) = repo_restricting(fb, "shared").await?;
        set_restricted_paths_logging(false);
        let commit = MappedCommit::with_files(
            &ctx,
            &repo,
            &[("dir/a", "a"), ("shared/s", "s")],
            &[("dir/b", "b")],
        )
        .await?;
        commit.store_root(&ctx, &repo).await?;
        let shared = commit
            .root_id()
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore().clone(),
                MPath::new("shared")?,
            )
            .await?
            .and_then(Entry::into_tree)
            .context("the child's root has shared")?;
        set_restricted_paths_knobs(true, fail_derivation);
        tracing.fail_next_get(shared.blobstore_key());

        // When: deriving the commit.
        let result = derive.root(&ctx, &repo, commit.child).await;

        // Then: the read error fails derivation exactly when failed
        // recordings do.
        let case = format!("{derive:?}, fail derivation {fail_derivation}");
        match result {
            Ok(root) => {
                assert!(!fail_derivation, "{case}: should fail");
                assert_eq!(root, commit.root_id(), "{case}");
            }
            Err(err) => {
                assert!(fail_derivation, "{case}: got {err:#}");
                assert!(
                    format!("{err:#}")
                        .contains(&format!("reading {} failed", shared.blobstore_key())),
                    "{case}: got {err:#}"
                );
            }
        }
    }
    Ok(())
}

/// A manifest-id store whose writes fail.
struct FailingManifestIdStore;

#[async_trait]
impl RestrictedPathsManifestIdStore for FailingManifestIdStore {
    async fn add_entry(
        &self,
        _ctx: &CoreContext,
        _entry: RestrictedPathManifestIdEntry,
    ) -> Result<bool> {
        bail!("the manifest-id store is down")
    }

    async fn add_entries(
        &self,
        _ctx: &CoreContext,
        _entries: &[RestrictedPathManifestIdEntry],
    ) -> Result<bool> {
        bail!("the manifest-id store is down")
    }

    async fn get_paths_by_manifest_id(
        &self,
        _ctx: &CoreContext,
        _manifest_id: &RestrictedManifestId,
        _manifest_type: &ManifestType,
    ) -> Result<Vec<NonRootMPath>> {
        Ok(vec![])
    }

    async fn get_all_entries(
        &self,
        _ctx: &CoreContext,
    ) -> Result<Vec<RestrictedPathManifestIdEntry>> {
        Ok(vec![])
    }

    async fn get_entries_by_id(
        &self,
        _ctx: &CoreContext,
        _min_id: u64,
    ) -> Result<Vec<RestrictedPathManifestIdEntry>> {
        Ok(vec![])
    }

    async fn get_all_paths_by_manifest_id(
        &self,
        _ctx: &CoreContext,
        _manifest_id: &RestrictedManifestId,
    ) -> Result<Vec<(ManifestType, NonRootMPath)>> {
        Ok(vec![])
    }

    async fn delete_by_manifest_id(
        &self,
        _ctx: &CoreContext,
        _manifest_id: &RestrictedManifestId,
    ) -> Result<u64> {
        Ok(0)
    }

    fn repo_id(&self) -> RepositoryId {
        RepositoryId::new(0)
    }
}

#[mononoke::fbinit_test]
async fn test_mapped_commit_fails_when_a_restricted_directory_write_must_fail(
    fb: FacebookInit,
) -> Result<()> {
    for derive in DERIVE_MODES {
        // Given: a usable stored root whose `dir` is restricted, in a repo
        // whose manifest-id store writes fail, with the JustKnob that makes
        // such failures fail derivation on.
        let ctx = CoreContext::test_mock(fb);
        let config_based = Arc::new(RestrictedPathsConfigBased::new(
            restricted_paths_config("dir")?,
            Arc::new(FailingManifestIdStore),
            None,
        ));
        let throwaway: Repo = test_repo_factory::build_empty(fb).await?;
        let restricted_paths = Arc::new(RestrictedPaths::new(
            "repo".to_string(),
            config_based,
            DummyAclProvider::new(fb)?,
            MononokeScubaSampleBuilder::with_discard(),
            throwaway.repo_derived_data_arc(),
        )?);
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_restricted_paths(restricted_paths)
            .build()
            .await?;
        set_restricted_paths_logging(false);
        let commit = MappedCommit::new(&ctx, &repo).await?;
        commit.store_root(&ctx, &repo).await?;
        override_just_knobs(JustKnobsInMemory::new(HashMap::from([
            (
                "scm/mononoke:enabled_restricted_paths_access_logging".to_string(),
                KnobVal::Bool(true),
            ),
            (
                "scm/mononoke:fail_derivation_on_restricted_manifest_id_store_error".to_string(),
                KnobVal::Bool(true),
            ),
        ])));

        // When: deriving the commit.
        let result = derive.root(&ctx, &repo, commit.child).await;

        // Then: the failed write fails derivation.
        let err = result.expect_err("the write failure should fail derivation");
        assert!(
            format!("{err:#}").contains("Failed to track restricted paths"),
            "{derive:?}: got {err:#}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_unmapped_commit_derives_from_bonsai(fb: FacebookInit) -> Result<()> {
    for derive in DERIVE_MODES {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("dir/b", "b")
            .commit()
            .await?;
        let parent_root = repo
            .repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&ctx, parent, DerivationPriority::LOW)
            .await?
            .hg_augmented_manifest_id();

        let root = derive.root(&ctx, &repo, child).await?;

        assert!(
            repo.bonsai_hg_mapping()
                .get_hg_from_bonsai(&ctx, child)
                .await?
                .is_none(),
            "{derive:?}: deriving the augmented manifest maps no Hg changeset"
        );
        let expected =
            derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai_changeset(
                &ctx,
                repo.repo_blobstore(),
                &child.load(&ctx, repo.repo_blobstore()).await?,
                vec![parent_root],
                repo.restricted_paths().config_based(),
                None,
            )
            .await?;
        assert_eq!(
            root, expected,
            "{derive:?}: an unmapped commit is built from its Bonsai changes"
        );
    }
    Ok(())
}
