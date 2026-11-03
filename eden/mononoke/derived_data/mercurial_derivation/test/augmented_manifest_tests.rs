/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use acl_manifest::DirectoryAclInputs;
use acl_manifest::RootAclManifestId;
use acl_manifest::acl_node_for_directory;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use cacheblob::MemWritesKeyedBlobstore;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::stream::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use justknobs::test_helpers::with_just_knobs_async;
use manifest::Entry;
use manifest::ManifestOps;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::augmented_backfill;
use mercurial_derivation::derive_hg_augmented_manifest;
use mercurial_derivation::upload_augmented_manifest::DirectoryAcl;
use mercurial_derivation::upload_augmented_manifest::UploadTreeBuildError;
use mercurial_derivation::upload_augmented_manifest::build_augmented_manifests_for_uploaded_trees;
use mercurial_derivation::upload_augmented_manifest::store_uploaded_tree_envelopes;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestEnvelope;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestEnvelope;
use mercurial_types::HgManifestId;
use mercurial_types::HgParents;
use mercurial_types::blobs::fetch_manifest_envelope;
use metaconfig_types::PathRestrictionMetadata;
use metaconfig_types::RestrictedPathsConfig;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::ContentMetadataV2Id;
use mononoke_types::FileChange;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::acl_manifest::AclManifestDirectoryRestriction;
use mononoke_types::acl_manifest::AclManifestEntry;
use mononoke_types::sharded_map_v2::ShardedMapV2Value;
use mononoke_types::typed_hash::AclManifestId;
use mononoke_types::typed_hash::BlobstoreKey;
use permission_checker::MononokeIdentity;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths::ManifestType;
use restricted_paths::RestrictedPathManifestIdEntry;
use restricted_paths::RestrictedPathsRef;
use tests_utils::CreateCommitContext;
use tests_utils::drawdag::extend_from_dag_with_actions;

use crate::Repo;

#[derive(Clone, Debug)]
enum DeniedGet {
    Keys(HashSet<String>),
    /// Denies a whole blobstore namespace, e.g. every `hgmanifest.sha1.*` key.
    Prefix(String),
}

impl DeniedGet {
    fn matches(&self, key: &str) -> bool {
        match self {
            Self::Keys(denied) => denied.contains(key),
            Self::Prefix(prefix) => key.starts_with(prefix.as_str()),
        }
    }
}

/// Whether a denied key is an assertion failure or a stand-in for a blob that
/// has not been written yet.
#[derive(Clone, Copy, Debug)]
enum DeniedGetResult {
    Error,
    Missing,
}

#[derive(Clone, Debug)]
struct DenyGetKeyedBlobstore<B> {
    inner: B,
    denied: DeniedGet,
    result: DeniedGetResult,
}

impl<B> DenyGetKeyedBlobstore<B> {
    fn new(inner: B, denied_key: String) -> Self {
        Self {
            inner,
            denied: DeniedGet::Keys(HashSet::from([denied_key])),
            result: DeniedGetResult::Error,
        }
    }

    fn new_denying_prefix(inner: B, denied_prefix: &str) -> Self {
        Self {
            inner,
            denied: DeniedGet::Prefix(denied_prefix.to_string()),
            result: DeniedGetResult::Error,
        }
    }

    fn missing(inner: B, denied_keys: impl IntoIterator<Item = String>) -> Self {
        Self {
            inner,
            denied: DeniedGet::Keys(denied_keys.into_iter().collect()),
            result: DeniedGetResult::Missing,
        }
    }

    fn denying_keys(inner: B, denied_keys: impl IntoIterator<Item = String>) -> Self {
        Self {
            inner,
            denied: DeniedGet::Keys(denied_keys.into_iter().collect()),
            result: DeniedGetResult::Error,
        }
    }
}

impl<B: fmt::Display> fmt::Display for DenyGetKeyedBlobstore<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DenyGetKeyedBlobstore<{}>", self.inner)
    }
}

#[async_trait]
impl<B: KeyedBlobstore + Clone> KeyedBlobstore for DenyGetKeyedBlobstore<B> {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        if self.denied.matches(key) {
            return match self.result {
                DeniedGetResult::Error => {
                    Err(anyhow!("unexpected load of denied blobstore key {key}"))
                }
                DeniedGetResult::Missing => Ok(None),
            };
        }
        self.inner.get(ctx, key).await
    }

    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()> {
        self.inner.put(ctx, key, value).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        self.inner.is_present(ctx, key).await
    }

    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        self.inner.copy(ctx, old_key, new_key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        self.inner.unlink(ctx, key).await
    }
}

/// Invariant test: after augmented manifest derivation, all HgManifest
/// blobs must be loadable via `Loadable`.
#[mononoke::fbinit_test]
async fn test_augmented_manifest_hg_blobs_loadable(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir_a/file", "1")
        .add_file("dir_b/file", "2")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir_a/file", "3")
        .add_file("dir_c/file", "4")
        .commit()
        .await?;

    let manager = repo.repo_derived_data().manager();

    // Pre-derive HgChangesets for the old HgManifest-based augmented path.
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, vec![root, child], None)
        .await?;

    // Pre-derive RootAclManifestId (batch dependency of RootHgAugmentedManifestId)
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root, child], None)
        .await?;

    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![root, child], None)
        .await?;

    // Verify ALL HgManifest blobs are loadable (root + subdirectory manifests).
    for cs_id in [root, child] {
        let hg_cs_id = repo.derive_hg_changeset(&ctx, cs_id).await?;
        let hg_mf_id = hg_cs_id
            .load(&ctx, repo.repo_blobstore())
            .await?
            .manifestid();
        // Load root manifest via Loadable
        let _root_mf = hg_mf_id.load(&ctx, repo.repo_blobstore()).await?;
        // Load all subtree manifests via list_all_entries
        let entries: Vec<_> = hg_mf_id
            .list_all_entries(ctx.clone(), repo.repo_blobstore().clone())
            .try_collect()
            .await?;
        // Sanity: should have both files and directories
        assert!(!entries.is_empty());
    }
    Ok(())
}

async fn get_manifests(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    parents: Vec<HgAugmentedManifestId>,
) -> Result<(HgManifestId, HgAugmentedManifestId)> {
    let hg_id = repo
        .derive_hg_changeset(ctx, cs_id)
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid();

    // Derive ACL manifest for this changeset to get the overlay.
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootAclManifestId>(ctx, vec![cs_id], None)
        .await?;
    let acl_root = manager
        .fetch_derived::<RootAclManifestId>(ctx, cs_id, None)
        .await?
        .unwrap_or_else(|| panic!("Missing RootAclManifestId for {cs_id}"));
    let acl_root_overlay = derive_hg_augmented_manifest::normalize_acl_root(&acl_root)?;

    // First derive the manifest in full using a temporary side blobstore.
    let blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let full_aug_id = augmented_backfill::derive_from_full_hg_manifest(
        ctx.clone(),
        blobstore.clone(),
        hg_id,
        acl_root_overlay,
    )
    .await?;
    let full_aug = full_aug_id.load(ctx, &blobstore).await?;

    let restricted_paths_config = repo.restricted_paths().config_based();
    // Now derive the manifest using the parents in the main blobstore.
    let aug_id = derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        repo.repo_blobstore(),
        hg_id,
        parents,
        &Default::default(),
        restricted_paths_config,
        acl_root_overlay,
    )
    .await?;
    let aug = aug_id.load(ctx, repo.repo_blobstore()).await?;

    // Verify ACL pointers match between full and parent-aware derivation
    assert_eq!(
        aug.augmented_manifest.acl_manifest_directory_id,
        full_aug.augmented_manifest.acl_manifest_directory_id,
        "acl_manifest_directory_id mismatch at root between full and parent-aware derivation"
    );

    // Check that the two manifests are the same (deep equality includes ACL pointers).
    assert_eq!(aug, full_aug);

    Ok((hg_id, aug_id))
}

async fn compare_manifests(
    ctx: &CoreContext,
    repo: &Repo,
    hg_id: HgManifestId,
    aug_id: HgAugmentedManifestId,
) -> Result<()> {
    let mut hg_e_entries: Vec<_> = hg_id
        .list_all_entries(ctx.clone(), repo.repo_blobstore().clone())
        .try_collect()
        .await?;
    let mut aug_e_entries: Vec<_> = aug_id
        .list_all_entries(ctx.clone(), repo.repo_blobstore().clone())
        .try_collect()
        .await?;

    hg_e_entries.sort_by_key(|(path, _)| path.clone());
    aug_e_entries.sort_by_key(|(path, _)| path.clone());

    assert_eq!(hg_e_entries.len(), aug_e_entries.len());
    for ((hg_path, hg_entry), (aug_path, aug_entry)) in
        hg_e_entries.iter().zip(aug_e_entries.iter())
    {
        assert_eq!(hg_path, aug_path);
        match (hg_entry, aug_entry) {
            (Entry::Tree(hg_tree), Entry::Tree(aug_tree)) => {
                assert_eq!(hg_tree.into_nodehash(), aug_tree.into_nodehash());
            }
            (Entry::Leaf((file_type, filenode)), Entry::Leaf(aug_leaf)) => {
                assert_eq!(file_type, &aug_leaf.file_type);
                assert_eq!(filenode.into_nodehash(), aug_leaf.filenode);
            }
            _ => {
                panic!("Mismatched entry types for {hg_path}: {hg_entry:?} vs {aug_entry:?}");
            }
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_augmented_manifest(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);

    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let (commits, _dag) = extend_from_dag_with_actions(
        &ctx,
        &repo,
        r#"
            A-B-C-E
             \   /
              -D-
            # default_files: false
            # modify: A animals "0"
            # modify: A black/tiger "1"
            # modify: A black/tortoise "2"
            # modify: A black/turtle "3"
            # modify: A black/falcon "4"
            # modify: A black/fox "5"
            # modify: A black/horse "6"
            # modify: A blue/ostrich "7"
            # modify: A blue/owl "8"
            # modify: A blue/penguin "9"
            # modify: A blue/rabbit "10"
            # modify: A blue/snake "11"
            # modify: A blue/whale "12"
            # modify: A brown/emu "13"
            # modify: A brown/iguana "14"
            # modify: A brown/koala "15"
            # modify: A brown/llama "16"
            # modify: A brown/panda "17"
            # modify: A brown/rhino "18"
            # modify: A brown/sloth "19"
            # modify: A brown/tiger "20"
            # modify: A orange/cat "21"
            # modify: A orange/dog "22"
            # modify: A orange/fish "23"
            # modify: A orange/giraffe "24"
            # modify: A orange/caterpillar "25"
            # modify: B black/turtle "26"
            # modify: B blue/owl "27"
            # modify: B blue/zebra "28"
            # modify: B orange/caterpillar "29"
            # delete: B black/tortoise
            # modify: C black/tiger "30"
            # delete: C brown/iguana
            # delete: C brown/koala
            # delete: C brown/llama
            # delete: C brown/panda
            # delete: C brown/rhino
            # delete: C brown/sloth
            # delete: C brown/tiger
            # modify: D red/albatross "30"
            # modify: D red/crow "31"
            # modify: D red/eagle "32"
            # modify: D black/falcon "33"
            # modify: E orange/caterpillar "29"
            # modify: E blue/owl "8"
            # modify: E blue/zebra "31"
            # modify: E black/falcon "33"
            # modify: E black/tiger "1"
            # delete: E black/turtle
            # delete: E black/tortoise
        "#,
    )
    .await?;

    let (hg_a, aug_a) = get_manifests(&ctx, &repo, commits["A"], vec![]).await?;
    let (hg_b, aug_b) = get_manifests(&ctx, &repo, commits["B"], vec![aug_a]).await?;
    let (hg_c, aug_c) = get_manifests(&ctx, &repo, commits["C"], vec![aug_b]).await?;
    let (hg_d, aug_d) = get_manifests(&ctx, &repo, commits["D"], vec![aug_a]).await?;
    let (hg_e, aug_e) = get_manifests(&ctx, &repo, commits["E"], vec![aug_c, aug_d]).await?;

    compare_manifests(&ctx, &repo, hg_a, aug_a).await?;
    compare_manifests(&ctx, &repo, hg_b, aug_b).await?;
    compare_manifests(&ctx, &repo, hg_c, aug_c).await?;
    compare_manifests(&ctx, &repo, hg_d, aug_d).await?;
    compare_manifests(&ctx, &repo, hg_e, aug_e).await?;

    Ok(())
}

/// Test that RootHgAugmentedManifestId batch derivation works correctly
/// across multiple batch segments. This exercises the scenario where
/// derive_heads_with_visited splits a large set of commits into multiple
/// calls to derive_exactly_batch.
#[mononoke::fbinit_test]
async fn test_augmented_manifest_multi_batch(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // Create a linear chain of commits. We'll split them into two batch
    // segments to simulate what derive_heads_with_visited does.
    let mut csids = Vec::new();
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file", "content_0")
        .commit()
        .await?;
    csids.push(root);

    for i in 1..10 {
        let parent = *csids.last().unwrap();
        let cs = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("file", format!("content_{i}"))
            .commit()
            .await?;
        csids.push(cs);
    }

    let manager = repo.repo_derived_data().manager();

    // Pre-derive dependencies of RootHgAugmentedManifestId.
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, csids.clone(), None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
        .await?;

    // Split into two batches and derive RootHgAugmentedManifestId.
    // First batch: commits 0..5
    let batch1 = csids[0..5].to_vec();
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, batch1, None)
        .await?;

    // Second batch: commits 5..10 (parent of commit 5 is commit 4, which
    // was in batch 1).
    let batch2 = csids[5..10].to_vec();
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, batch2, None)
        .await?;

    // Verify all derived augmented manifests match full derivation
    let derived = manager
        .fetch_derived_batch::<RootHgAugmentedManifestId>(&ctx, csids.clone(), None)
        .await?;

    for cs_id in &csids {
        let aug_id = derived
            .get(cs_id)
            .unwrap_or_else(|| panic!("Missing RootHgAugmentedManifestId for {cs_id}"))
            .hg_augmented_manifest_id();

        let hg_cs_id = repo.derive_hg_changeset(&ctx, *cs_id).await?;
        let hg_manifest_id = hg_cs_id
            .load(&ctx, repo.repo_blobstore())
            .await?
            .manifestid();

        compare_manifests(&ctx, &repo, hg_manifest_id, aug_id).await?;
    }

    Ok(())
}

/// Test that RootHgAugmentedManifestId derivation via derive_heads works
/// correctly on the old HgManifest-based path. `derive_heads` derives
/// declared dependencies first; disabling direct derivation keeps this
/// coverage on the old path after the test JK default switches to direct
/// derivation.
#[mononoke::fbinit_test]
async fn test_augmented_manifest_derive_heads(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // Create a linear chain of 3 commits. derive_heads will process
    // these via derive_exactly_batch -> derive_batch.
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file", "root")
        .commit()
        .await?;

    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file", "child")
        .commit()
        .await?;

    let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
        .add_file("file", "grandchild")
        .commit()
        .await?;

    let manager = repo.repo_derived_data().manager();

    // Given: do not manually prederive MappedHgChangesetId; derive_heads derives
    // declared dependencies before deriving RootHgAugmentedManifestId.
    //
    // When: deriving v1 through derive_heads.
    manager
        .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![grandchild], None, None)
        .await?;

    // Then: all augmented manifests were derived correctly.
    for cs_id in [root, child, grandchild] {
        let aug = manager
            .fetch_derived::<RootHgAugmentedManifestId>(&ctx, cs_id, None)
            .await?
            .unwrap_or_else(|| panic!("Missing RootHgAugmentedManifestId for {cs_id}"));

        let hg_cs_id = repo.derive_hg_changeset(&ctx, cs_id).await?;
        let hg_manifest_id = hg_cs_id
            .load(&ctx, repo.repo_blobstore())
            .await?
            .manifestid();

        compare_manifests(&ctx, &repo, hg_manifest_id, aug.hg_augmented_manifest_id()).await?;
    }

    Ok(())
}

/// Test that the old HgManifest-based augmented-manifest path remains
/// compatible when `hgmanifest_skip_writes=true`.
#[mononoke::fbinit_test]
async fn test_augmented_manifest_skip_writes(fb: FacebookInit) -> Result<()> {
    override_just_knobs(JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke:hgmanifest_skip_writes".to_string(),
        KnobVal::Bool(true),
    )])));

    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file_a", "content_a")
        .add_file("dir/file_b", "content_b")
        .add_file("other/file_c", "content_c")
        .commit()
        .await?;

    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir/file_a", "updated_a")
        .add_file("new/file_d", "content_d")
        .commit()
        .await?;

    let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
        .add_file("other/file_c", "updated_c")
        .commit()
        .await?;

    let manager = repo.repo_derived_data().manager();

    // Prederive the static manager dependency retained in this stack.
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, vec![root, child, grandchild], None)
        .await?;

    // Pre-derive RootAclManifestId (batch dependency of RootHgAugmentedManifestId)
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root, child, grandchild], None)
        .await?;

    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(
            &ctx,
            vec![root, child, grandchild],
            None,
        )
        .await?;

    for cs_id in [root, child, grandchild] {
        let aug = manager
            .fetch_derived::<RootHgAugmentedManifestId>(&ctx, cs_id, None)
            .await?
            .unwrap_or_else(|| panic!("Missing RootHgAugmentedManifestId for {cs_id}"));

        let hg_cs_id = repo.derive_hg_changeset(&ctx, cs_id).await?;
        let hg_manifest_id = hg_cs_id
            .load(&ctx, repo.repo_blobstore())
            .await?
            .manifestid();

        // HgManifest blobs are loadable via the reconstruction layer.
        let _root_mf = hg_manifest_id.load(&ctx, repo.repo_blobstore()).await?;

        let entries: Vec<_> = hg_manifest_id
            .list_all_entries(ctx.clone(), repo.repo_blobstore().clone())
            .try_collect()
            .await?;
        assert!(!entries.is_empty());

        compare_manifests(&ctx, &repo, hg_manifest_id, aug.hg_augmented_manifest_id()).await?;
    }

    Ok(())
}

/// Test that augmented manifest derivation produces identical results
/// from both the parent-aware and full derivation paths when the repo
/// has .slacl files (non-empty AclManifest).
#[mononoke::fbinit_test]
async fn test_augmented_manifest_parity_with_slacl(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // Root commit with a .slacl file creating a restriction root.
    // ACL tree: root (waypoint) -> restricted (waypoint) -> code (restriction root)
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/code/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/code/secret.rs", "fn secret() {}")
        .add_file("public/readme.md", "hello")
        .commit()
        .await?;

    // Derive and compare via the parity helper — this should verify that
    // both derivation paths produce identical augmented manifests,
    // including acl_manifest_directory_id pointers.
    let (_, aug_root) = get_manifests(&ctx, &repo, root, vec![]).await?;

    // Child commit adding more files (tests incremental vs full parity
    // when parent manifests exist and subtrees are reused)
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("restricted/code/more.rs", "fn more() {}")
        .add_file("public/docs.md", "docs")
        .commit()
        .await?;

    let (_, _aug_child) = get_manifests(&ctx, &repo, child, vec![aug_root]).await?;

    Ok(())
}

/// Returns `[root, left, right, merge]`. Building the same history in two
/// repos yields the same augmented manifests, because Hg node ids depend only
/// on file history, not on changeset ids.
async fn create_backfill_parity_history(
    ctx: &CoreContext,
    repo: &Repo,
) -> Result<[ChangesetId; 4]> {
    let root = CreateCommitContext::new_root(ctx, repo)
        .add_file(
            "restricted/code/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/code/secret.rs", "fn secret() {}")
        .add_file("public/readme.md", "hello")
        .add_file("public/old.md", "old")
        .commit()
        .await?;
    let left = CreateCommitContext::new(ctx, repo, vec![root])
        .add_file("restricted/code/more.rs", "fn more() {}")
        .add_file_with_copy_info("public/copied.md", "hello", (root, "public/readme.md"))
        .delete_file("public/old.md")
        .commit()
        .await?;
    let right = CreateCommitContext::new(ctx, repo, vec![root])
        .add_file("public/readme.md", "hello again")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(ctx, repo, vec![left, right])
        .add_file("public/readme.md", "hello again")
        .delete_file("public/old.md")
        .commit()
        .await?;
    Ok([root, left, right, merge])
}

/// Loads every directory envelope under `root`, keyed by path. Envelope digests
/// omit ACL pointers, so comparing only root envelopes would miss a wrong
/// pointer deeper in the tree.
async fn load_augmented_directory_envelopes<B: KeyedBlobstore + Clone + 'static>(
    ctx: &CoreContext,
    store: &B,
    root: HgAugmentedManifestId,
) -> Result<BTreeMap<MPath, HgAugmentedManifestEnvelope>> {
    let directories: Vec<_> = root
        .list_all_entries(ctx.clone(), store.clone())
        .try_filter_map(|(path, entry)| async move { Ok(entry.into_tree().map(|id| (path, id))) })
        .try_collect()
        .await?;
    let mut envelopes = BTreeMap::new();
    for (path, id) in directories {
        envelopes.insert(path, id.load(ctx, store).await?);
    }
    Ok(envelopes)
}

/// Backfill rebuilds each augmented manifest from the commit's full Hg
/// manifest, without parent augmented manifests. Backfill and V2 publish the
/// same shared root, so they must produce identical manifests.
#[mononoke::fbinit_test]
async fn test_augmented_manifest_backfill_matches_v2_derivation(fb: FacebookInit) -> Result<()> {
    // Given: the same history, with an ACL restriction root, a copy, a
    // deletion and a merge, in two repos. In the first, V2 derived every
    // augmented manifest from Bonsai alone. The second has none derived.
    let ctx = CoreContext::test_mock(fb);
    let v2_repo: Repo = test_repo_factory::build_empty(fb).await?;
    let v2_csids = create_backfill_parity_history(&ctx, &v2_repo).await?;
    let v2_manager = v2_repo.repo_derived_data().manager();
    v2_manager
        .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![v2_csids[3]], None, None)
        .await?;
    assert!(
        v2_repo
            .bonsai_hg_mapping()
            .get(&ctx, v2_csids.to_vec().into())
            .await?
            .is_empty(),
        "V2 must derive from Bonsai, not from mapped Hg manifests",
    );
    let v2_roots = v2_manager
        .fetch_derived_batch::<RootHgAugmentedManifestId>(&ctx, v2_csids.to_vec(), None)
        .await?;

    let backfill_repo: Repo = test_repo_factory::build_empty(fb).await?;
    let backfill_csids = create_backfill_parity_history(&ctx, &backfill_repo).await?;
    let backfill_manager = backfill_repo.repo_derived_data().manager();
    assert!(
        backfill_manager
            .fetch_derived_batch::<RootHgAugmentedManifestId>(&ctx, backfill_csids.to_vec(), None)
            .await?
            .is_empty(),
        "backfill would fetch an existing root instead of deriving one",
    );

    // When: backfilling every commit in the second repo, head first.
    let mut backfilled_roots = HashMap::new();
    for csid in backfill_csids.into_iter().rev() {
        let root = backfill_manager
            .unsafe_derive_untopologically::<RootHgAugmentedManifestId>(&ctx, csid, None)
            .await?;
        backfilled_roots.insert(csid, root.hg_augmented_manifest_id());
    }

    // Then: each backfilled root, and every directory envelope under it
    // including ACL pointers, equals the V2 result.
    for (v2_csid, backfill_csid) in v2_csids.into_iter().zip(backfill_csids) {
        let v2_root = v2_roots
            .get(&v2_csid)
            .unwrap_or_else(|| panic!("Missing RootHgAugmentedManifestId for {v2_csid}"))
            .hg_augmented_manifest_id();
        let v2_envelopes =
            load_augmented_directory_envelopes(&ctx, v2_repo.repo_blobstore(), v2_root).await?;
        assert!(
            v2_envelopes[&MPath::new("restricted/code")?]
                .augmented_manifest
                .acl_manifest_directory_id
                .is_some(),
            "fixture must have an ACL pointer below the root",
        );
        let backfilled_root = backfilled_roots[&backfill_csid];
        assert_eq!(backfilled_root, v2_root);
        assert_eq!(
            load_augmented_directory_envelopes(
                &ctx,
                backfill_repo.repo_blobstore(),
                backfilled_root,
            )
            .await?,
            v2_envelopes,
            "backfilled augmented manifest for {backfill_csid} should match V2",
        );
    }

    Ok(())
}

/// Test that resolve_copy_from_filenodes correctly resolves copy-from
/// source filenodes from parent augmented manifests.
#[mononoke::fbinit_test]
async fn test_resolve_copy_from_filenodes(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // Create a root commit with a file
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("original_file", "content")
        .commit()
        .await?;

    // Create a child that copies the file
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file_with_copy_info("copied_file", "content", (root, "original_file"))
        .commit()
        .await?;

    // Derive HgChangesets and augmented manifests for the root (parent)
    let manager = repo.repo_derived_data().manager();

    // Pre-derive HgChangesets for the old HgManifest-based augmented path.
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, vec![root], None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root], None)
        .await?;

    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![root], None)
        .await?;

    let root_aug = manager
        .fetch_derived::<RootHgAugmentedManifestId>(&ctx, root, None)
        .await?
        .expect("Missing augmented manifest for root")
        .hg_augmented_manifest_id();

    // Build file_changes for the child commit
    let child_bonsai: mononoke_types::BonsaiChangeset =
        child.load(&ctx, repo.repo_blobstore()).await?;
    let file_changes: Vec<_> = child_bonsai
        .file_changes()
        .map(|(path, fc)| {
            let tc = match fc {
                FileChange::Change(tc) => Some(tc.clone()),
                FileChange::Deletion
                | FileChange::UntrackedChange(_)
                | FileChange::UntrackedDeletion => None,
            };
            (path.clone(), tc)
        })
        .collect();

    // Resolve copy-from filenodes
    let result = derive_hg_augmented_manifest::resolve_copy_from_filenodes(
        &ctx,
        repo.repo_blobstore(),
        &MPath::new("copied_file")?,
        &file_changes,
        (Some(root), None),
        &[],
        &HashMap::from([(root, root_aug)]),
    )
    .await?;

    // Should have resolved the copy-from for "copied_file" -> "original_file"
    let original_path = NonRootMPath::new("original_file")?;
    assert!(
        result.contains_key(&(original_path.clone(), root)),
        "Should resolve copy-from for original_file in parent {root}",
    );

    // Verify the resolved filenode matches the actual filenode in the parent HgManifest
    let hg_cs_id = repo.derive_hg_changeset(&ctx, root).await?;
    let hg_manifest_id = hg_cs_id
        .load(&ctx, repo.repo_blobstore())
        .await?
        .manifestid();

    let expected_entries: Vec<_> = hg_manifest_id
        .find_entries(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            vec![manifest::PathOrPrefix::Path(original_path.clone().into())],
        )
        .try_collect()
        .await?;

    let expected_filenode = match &expected_entries[0].1 {
        Entry::Leaf((_, filenode)) => *filenode,
        _ => panic!("Expected a leaf entry"),
    };

    assert_eq!(
        result[&(original_path, root)],
        expected_filenode,
        "Resolved filenode should match HgManifest filenode"
    );

    Ok(())
}

/// Test that resolve_copy_from_filenodes skips (rather than errors) when
/// the copy-from source path doesn't exist in the parent manifest.
/// This matches the behavior of the HgManifest-based derivation path in
/// derive_hg_changeset.rs.
#[mononoke::fbinit_test]
async fn test_resolve_copy_from_filenodes_missing_source(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // Create root with file.txt
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", "content")
        .commit()
        .await?;

    // Delete file.txt in the child
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("file.txt")
        .commit()
        .await?;

    // Create a grandchild that claims to copy from file.txt at the child
    // (where file.txt no longer exists)
    let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
        .add_file_with_copy_info("new_file", "content", (child, "file.txt"))
        .commit()
        .await?;

    let manager = repo.repo_derived_data().manager();

    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, vec![root, child], None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root, child], None)
        .await?;
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![root, child], None)
        .await?;

    let child_aug = manager
        .fetch_derived::<RootHgAugmentedManifestId>(&ctx, child, None)
        .await?
        .expect("Missing augmented manifest for child")
        .hg_augmented_manifest_id();

    let grandchild_bonsai: mononoke_types::BonsaiChangeset =
        grandchild.load(&ctx, repo.repo_blobstore()).await?;
    let file_changes: Vec<_> = grandchild_bonsai
        .file_changes()
        .map(|(path, fc)| {
            let tc = match fc {
                FileChange::Change(tc) => Some(tc.clone()),
                FileChange::Deletion
                | FileChange::UntrackedChange(_)
                | FileChange::UntrackedDeletion => None,
            };
            (path.clone(), tc)
        })
        .collect();

    let result = derive_hg_augmented_manifest::resolve_copy_from_filenodes(
        &ctx,
        repo.repo_blobstore(),
        &MPath::new("new_file")?,
        &file_changes,
        (Some(child), None),
        &[],
        &HashMap::from([(child, child_aug)]),
    )
    .await?;

    // The copy-from entry should be absent (skipped) since file.txt
    // doesn't exist in the child's manifest.
    let missing_path = NonRootMPath::new("file.txt")?;
    assert!(
        !result.contains_key(&(missing_path, child)),
        "Should skip copy-from when source path is missing from parent manifest"
    );

    Ok(())
}

/// Verify the streaming `compute_hg_node_id` produces the same hash as the
/// materialising `serialize_manifest` + `calculate_hg_node_id` reference path.
///
/// This is the correctness contract that lets the new direct-derivation path
/// avoid `try_collect`-ing every entry into memory before hashing — a pattern
/// that has caused OOMs on huge directories like `fbcode/third-party`.
#[mononoke::fbinit_test]
async fn test_compute_hg_node_id_matches_materialised(fb: FacebookInit) -> Result<()> {
    use futures::stream::TryStreamExt;
    use mercurial_types::HgAugmentedManifestEntry;
    use mercurial_types::HgParents;
    use mercurial_types::calculate_hg_node_id;
    use mercurial_types::preloaded_augmented_manifest::serialize_manifest;
    use mononoke_types::MPathElement;
    use repo_blobstore::RepoBlobstoreRef;

    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // Build a real commit so we get a real, populated augmented-manifest envelope.
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("alpha", "a")
        .add_file("beta", "b")
        .add_file("subdir/file1", "c")
        .add_file("subdir/file2", "d")
        .add_file("zeta", "e")
        .commit()
        .await?;

    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, vec![cs_id], None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![cs_id], None)
        .await?;
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![cs_id], None)
        .await?;
    let aug_root = manager
        .fetch_derived::<RootHgAugmentedManifestId>(&ctx, cs_id, None)
        .await?
        .expect("derived just above");
    let aug_id = aug_root.hg_augmented_manifest_id();
    let envelope = aug_id.load(&ctx, repo.repo_blobstore()).await?;

    let subentries = envelope.augmented_manifest.subentries.clone();
    let parents = HgParents::new(
        envelope.augmented_manifest.p1,
        envelope.augmented_manifest.p2,
    );

    // Streaming path under test.
    let streaming = derive_hg_augmented_manifest::compute_hg_node_id(
        subentries.clone(),
        &ctx,
        repo.repo_blobstore(),
        &parents,
    )
    .await?;

    // Reference: collect all entries into a Vec, serialise the directory in
    // one go, then hash the assembled bytes via the non-streaming variant.
    let collected: Vec<(MPathElement, HgAugmentedManifestEntry)> = subentries
        .into_entries(&ctx, repo.repo_blobstore())
        .and_then(|(path, entry)| async move { Ok((MPathElement::from_smallvec(path)?, entry)) })
        .try_collect()
        .await?;
    let materialised = serialize_manifest(&collected)?;
    let reference = calculate_hg_node_id(materialised.as_ref(), &parents);

    assert_eq!(
        streaming, reference,
        "streaming compute_hg_node_id must match materialised serialize_manifest + calculate_hg_node_id"
    );
    // And both must equal the canonical hg_node_id stored in the envelope.
    assert_eq!(
        streaming, envelope.augmented_manifest.hg_node_id,
        "streaming compute_hg_node_id must match the canonical hg_node_id stored on the envelope"
    );

    // Cross-check: must also match the hg_node_id that the existing HgManifest
    // derivation path produces for the same commit. This is the contract that
    // lets the new direct-derivation path be a drop-in for HgManifest derivation.
    let hg_cs_id = repo.derive_hg_changeset(&ctx, cs_id).await?;
    let hg_manifest_id = hg_cs_id
        .load(&ctx, repo.repo_blobstore())
        .await?
        .manifestid();
    assert_eq!(
        streaming,
        hg_manifest_id.into_nodehash(),
        "streaming compute_hg_node_id must match HgManifest derivation"
    );

    Ok(())
}

async fn derive_augmented_manifests(
    ctx: &CoreContext,
    repo: &Repo,
    cs_ids: Vec<ChangesetId>,
) -> Result<Vec<(HgAugmentedManifestId, HgAugmentedManifestEnvelope)>> {
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(ctx, cs_ids.clone(), None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(ctx, cs_ids.clone(), None)
        .await?;
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, cs_ids.clone(), None)
        .await?;

    let mut manifests = Vec::new();
    for cs_id in cs_ids {
        let aug_id = manager
            .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
            .await?
            .unwrap_or_else(|| panic!("Missing RootHgAugmentedManifestId for {cs_id}"))
            .hg_augmented_manifest_id();
        let envelope = aug_id.load(ctx, repo.repo_blobstore()).await?;
        manifests.push((aug_id, envelope));
    }

    Ok(manifests)
}

#[mononoke::fbinit_test]
async fn test_try_reuse_parent_envelope_reuses_matching_parent(fb: FacebookInit) -> Result<()> {
    // Given: merged subentries serialise to the same content as the parent.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1_cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("alpha", "1")
        .add_file("beta", "2")
        .add_file("gamma", "3")
        .commit()
        .await?;
    let mut manifests = derive_augmented_manifests(&ctx, &repo, vec![p1_cs]).await?;
    let (p1_aug_id, p1_env) = manifests.pop().expect("derived one augmented manifest");
    let p1_only_parents = HgParents::new(Some(p1_aug_id.into_nodehash()), None);

    // When: probing parent-envelope reuse with the matching parent as p1.
    let reuse = derive_hg_augmented_manifest::try_reuse_parent_envelope(
        &ctx,
        repo.repo_blobstore(),
        p1_env.augmented_manifest.subentries.clone(),
        Some(p1_aug_id),
        None,
        p1_only_parents,
    )
    .await?;

    // Then: p1 is reused and the fresh-envelope hash is still available.
    let expected_fresh_node_id = derive_hg_augmented_manifest::compute_hg_node_id(
        p1_env.augmented_manifest.subentries.clone(),
        &ctx,
        repo.repo_blobstore(),
        &p1_only_parents,
    )
    .await?;
    assert_eq!(
        reuse.reusable_parent,
        Some(p1_aug_id),
        "should reuse the matching parent"
    );
    assert_eq!(reuse.fresh_computed_node_id, expected_fresh_node_id);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_try_reuse_parent_envelope_creates_fresh_for_different_content(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a candidate parent and merged subentries from unrelated content.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1_cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("alpha", "1")
        .add_file("beta", "2")
        .add_file("gamma", "3")
        .commit()
        .await?;
    let other_cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("delta", "4")
        .add_file("epsilon", "5")
        .commit()
        .await?;
    let mut manifests = derive_augmented_manifests(&ctx, &repo, vec![p1_cs, other_cs])
        .await?
        .into_iter();
    let (p1_aug_id, _p1_env) = manifests.next().expect("derived p1 augmented manifest");
    let (_other_aug_id, other_env) = manifests.next().expect("derived other augmented manifest");
    let p1_only_parents = HgParents::new(Some(p1_aug_id.into_nodehash()), None);

    // When: probing reuse for subentries that do not match the candidate parent.
    let reuse = derive_hg_augmented_manifest::try_reuse_parent_envelope(
        &ctx,
        repo.repo_blobstore(),
        other_env.augmented_manifest.subentries.clone(),
        Some(p1_aug_id),
        None,
        p1_only_parents,
    )
    .await?;

    // Then: no parent is reused and the fresh-envelope hash is returned.
    let expected_fresh_node_id = derive_hg_augmented_manifest::compute_hg_node_id(
        other_env.augmented_manifest.subentries.clone(),
        &ctx,
        repo.repo_blobstore(),
        &p1_only_parents,
    )
    .await?;
    assert_eq!(
        reuse.reusable_parent, None,
        "merged subentries differ from parent; expected fresh envelope"
    );
    assert_eq!(reuse.fresh_computed_node_id, expected_fresh_node_id);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_try_reuse_parent_envelope_reuses_second_parent_when_first_does_not_match(
    fb: FacebookInit,
) -> Result<()> {
    // Given: two candidate parents where only p2 matches the merged subentries.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1_cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("alpha", "1")
        .add_file("beta", "2")
        .add_file("gamma", "3")
        .commit()
        .await?;
    let p2_cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("delta", "4")
        .add_file("epsilon", "5")
        .commit()
        .await?;
    let mut manifests = derive_augmented_manifests(&ctx, &repo, vec![p1_cs, p2_cs])
        .await?
        .into_iter();
    let (p1_aug_id, _p1_env) = manifests.next().expect("derived p1 augmented manifest");
    let (p2_aug_id, p2_env) = manifests.next().expect("derived p2 augmented manifest");
    let merge_parents = HgParents::new(
        Some(p1_aug_id.into_nodehash()),
        Some(p2_aug_id.into_nodehash()),
    );

    // When: probing reuse for subentries that match only p2.
    let reuse = derive_hg_augmented_manifest::try_reuse_parent_envelope(
        &ctx,
        repo.repo_blobstore(),
        p2_env.augmented_manifest.subentries.clone(),
        Some(p1_aug_id),
        Some(p2_aug_id),
        merge_parents,
    )
    .await?;

    // Then: p2 is reused and the fresh-envelope hash is still available.
    let expected_fresh_node_id = derive_hg_augmented_manifest::compute_hg_node_id(
        p2_env.augmented_manifest.subentries.clone(),
        &ctx,
        repo.repo_blobstore(),
        &merge_parents,
    )
    .await?;
    assert_eq!(
        reuse.reusable_parent,
        Some(p2_aug_id),
        "should reuse the second parent that matches"
    );
    assert_eq!(reuse.fresh_computed_node_id, expected_fresh_node_id);

    Ok(())
}

// -----------------------------------------------------------------------------
// Generic tests for the direct augmented-manifest derivation entry point.
// -----------------------------------------------------------------------------

async fn file_changes_from_bonsai(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<Vec<(NonRootMPath, Option<mononoke_types::TrackedFileChange>)>> {
    let bonsai: mononoke_types::BonsaiChangeset = cs_id.load(ctx, repo.repo_blobstore()).await?;
    Ok(bonsai
        .file_changes()
        .map(|(path, file_change)| {
            let tracked_change = match file_change {
                FileChange::Change(tracked_change) => Some(tracked_change.clone()),
                FileChange::Deletion => None,
                _ => None,
            };
            (path.clone(), tracked_change)
        })
        .collect())
}

/// Derive `RootAclManifestId` for `cs_id` and normalize it into the
/// `Option<AclManifestId>` overlay shape that the augmented-manifest
/// derivation entry points expect. Idempotent on repeat calls.
async fn derive_acl_overlay(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<Option<AclManifestId>> {
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootAclManifestId>(ctx, vec![cs_id], None)
        .await?;
    let acl_root = manager
        .fetch_derived::<RootAclManifestId>(ctx, cs_id, None)
        .await?
        .unwrap_or_else(|| panic!("Missing RootAclManifestId for {cs_id}"));
    derive_hg_augmented_manifest::normalize_acl_root(&acl_root)
}

async fn derive_augmented_manifest_via_existing_path(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    parents: Vec<HgAugmentedManifestId>,
    acl_root_overlay: Option<AclManifestId>,
) -> Result<(HgManifestId, HgAugmentedManifestId)> {
    let hg_manifest_id = repo
        .derive_hg_changeset(ctx, cs_id)
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid();
    let restricted_paths_config = repo.restricted_paths().config_based();
    let augmented_manifest_id = derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        repo.repo_blobstore(),
        hg_manifest_id,
        parents,
        &Default::default(),
        restricted_paths_config,
        acl_root_overlay,
    )
    .await?;

    Ok((hg_manifest_id, augmented_manifest_id))
}

async fn assert_direct_derive_matches_existing_path(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    augmented_parents: &[HgAugmentedManifestId],
    bonsai_parents: (Option<ChangesetId>, Option<ChangesetId>),
) -> Result<HgAugmentedManifestId> {
    assert_dual_derive_agree(ctx, repo, cs_id, augmented_parents, bonsai_parents).await
}

/// Derive the augmented manifest via both paths and assert they agree.
async fn assert_dual_derive_agree(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    aug_parents: &[HgAugmentedManifestId],
    bonsai_parents: (Option<ChangesetId>, Option<ChangesetId>),
) -> Result<HgAugmentedManifestId> {
    let acl_root_overlay = derive_acl_overlay(ctx, repo, cs_id).await?;
    let (hg_manifest_id, via_existing_path) = derive_augmented_manifest_via_existing_path(
        ctx,
        repo,
        cs_id,
        aug_parents.to_vec(),
        acl_root_overlay,
    )
    .await?;
    let file_changes = file_changes_from_bonsai(ctx, repo, cs_id).await?;
    let restricted_paths_config = repo.restricted_paths().config_based();

    let via_direct_path = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        ctx,
        repo.repo_blobstore(),
        aug_parents.to_vec(),
        file_changes,
        bonsai_parents,
        &Default::default(),
        restricted_paths_config,
        acl_root_overlay,
    )
    .await?;

    assert_eq!(
        via_direct_path, via_existing_path,
        "direct augmented-manifest derivation should match existing HgManifest-based derivation for {cs_id}",
    );

    let direct_envelope = via_direct_path.load(ctx, repo.repo_blobstore()).await?;
    let existing_envelope = via_existing_path.load(ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        direct_envelope.augmented_manifest.hg_node_id,
        existing_envelope.augmented_manifest.hg_node_id,
        "root hg_node_id should match for {cs_id}",
    );
    assert_eq!(
        direct_envelope.augmented_manifest.p1, existing_envelope.augmented_manifest.p1,
        "root p1 should match for {cs_id}",
    );
    assert_eq!(
        direct_envelope.augmented_manifest.p2, existing_envelope.augmented_manifest.p2,
        "root p2 should match for {cs_id}",
    );
    compare_manifests(ctx, repo, hg_manifest_id, via_direct_path).await?;

    Ok(via_direct_path)
}

fn restricted_paths_access_logging_knobs(enabled: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke:enabled_restricted_paths_access_logging".to_string(),
        KnobVal::Bool(enabled),
    )]))
}

async fn build_repo_with_restricted_path_config(
    fb: FacebookInit,
    restricted_paths: Vec<NonRootMPath>,
) -> Result<Repo> {
    let path_restriction_metadata = restricted_paths
        .into_iter()
        .map(|path| {
            (
                path,
                PathRestrictionMetadata {
                    repo_region_acl: MononokeIdentity::from_legacy_type_data(
                        "REPO_REGION",
                        "test_acl",
                    ),
                    permission_request_group: None,
                    rollout_allowlist_group: None,
                    read_only: false,
                },
            )
        })
        .collect();

    let repo = test_repo_factory::TestRepoFactory::new(fb)?
        .with_config_override(move |cfg| {
            cfg.restricted_paths_config = RestrictedPathsConfig {
                path_restriction_metadata,
                ..Default::default()
            };
        })
        .build()
        .await?;
    Ok(repo)
}

async fn create_restricted_path_test_stack(
    ctx: &CoreContext,
    repo: &Repo,
) -> Result<(ChangesetId, ChangesetId)> {
    let parent = CreateCommitContext::new_root(ctx, repo)
        .add_file("restricted/secret.txt", "hidden")
        .add_file("public/normal.txt", "visible")
        .commit()
        .await?;
    let child = CreateCommitContext::new(ctx, repo, vec![parent])
        .add_file("restricted/secret.txt", "hidden v2")
        .add_file("restricted/nested/deep.txt", "deeper")
        .add_file("public/normal.txt", "visible v2")
        .commit()
        .await?;

    Ok((parent, child))
}

async fn derive_augmented_manifest_directly_for_test(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    parents: Vec<HgAugmentedManifestId>,
    bonsai_parents: (Option<ChangesetId>, Option<ChangesetId>),
) -> Result<HgAugmentedManifestId> {
    let file_changes = file_changes_from_bonsai(ctx, repo, cs_id).await?;
    let restricted_paths_config = repo.restricted_paths().config_based();

    derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        ctx,
        repo.repo_blobstore(),
        parents,
        file_changes,
        bonsai_parents,
        &Default::default(),
        restricted_paths_config,
        None,
    )
    .await
}

async fn lookup_augmented_root_child(
    ctx: &CoreContext,
    repo: &Repo,
    root: HgAugmentedManifestId,
    child: &[u8],
) -> Result<Option<HgAugmentedManifestEntry>> {
    root.load(ctx, repo.repo_blobstore())
        .await?
        .augmented_manifest
        .lookup(
            ctx,
            repo.repo_blobstore(),
            &MPathElement::new_from_slice(child)?,
        )
        .await
}

async fn store_augmented_manifest_with_supplied_root(
    ctx: &CoreContext,
    repo: &Repo,
    source_id: HgAugmentedManifestId,
    supplied: mercurial_types::HgNodeHash,
) -> Result<HgAugmentedManifestId> {
    let mut envelope = source_id.load(ctx, repo.repo_blobstore()).await?;
    envelope.augmented_manifest.hg_node_id = supplied;
    let (augmented_manifest_id, augmented_manifest_size) = envelope
        .augmented_manifest
        .clone()
        .compute_content_addressed_digest(ctx, repo.repo_blobstore())
        .await?;
    envelope.augmented_manifest_id = augmented_manifest_id;
    envelope.augmented_manifest_size = augmented_manifest_size;
    envelope.store(ctx, repo.repo_blobstore()).await
}

async fn hg_augmented_restricted_path_entries(
    ctx: &CoreContext,
    repo: &Repo,
) -> Result<Vec<RestrictedPathManifestIdEntry>> {
    let mut entries: Vec<_> = repo
        .restricted_paths()
        .config_based()
        .manifest_id_store()
        .get_all_entries(ctx)
        .await?
        .into_iter()
        .filter(|entry| entry.manifest_type == ManifestType::HgAugmented)
        .collect();
    entries.sort();
    Ok(entries)
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_matches_existing_path_for_root_commit(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a root commit with both file and directory entries.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("README.md", "hello")
        .add_file("src/lib.rs", "pub fn value() -> u8 { 1 }")
        .commit()
        .await?;

    // When: deriving its augmented manifest directly from Bonsai.
    let direct_id =
        assert_direct_derive_matches_existing_path(&ctx, &repo, commit, &[], (None, None)).await?;

    // Then: the direct path produces the same root augmented manifest as the
    // existing HgManifest-based path.
    assert!(
        direct_id.load(&ctx, repo.repo_blobstore()).await.is_ok(),
        "directly derived root envelope should be loadable",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_entry_matches_canonical_non_root_acl_entry(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a root commit whose non-root stage has a nested ACL subtree.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("README.md", "hello")
        .add_file(
            "src/restricted/.slacl",
            b"repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("src/restricted/lib.rs", "pub fn value() -> u8 { 1 }")
        .commit()
        .await?;
    let root_acl_id = derive_acl_overlay(&ctx, &repo, commit)
        .await?
        .context("fixture must have a non-empty ACL manifest")?;
    let stage_path = MPath::new("src")?;
    let stage_acl_id = root_acl_id
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            stage_path.clone(),
        )
        .await?
        .and_then(Entry::into_tree)
        .context("fixture must have an ACL tree at src")?;
    let canonical_root = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        vec![],
        file_changes_from_bonsai(&ctx, &repo, commit).await?,
        (None, None),
        &Default::default(),
        repo.restricted_paths().config_based(),
        Some(root_acl_id),
    )
    .await?;
    let expected_entry = lookup_augmented_root_child(&ctx, &repo, canonical_root, b"src")
        .await?
        .context("fixture must contain src")?;
    let HgAugmentedManifestEntry::DirectoryNode(expected_dir) = &expected_entry else {
        return Err(anyhow!("fixture src entry must be a directory"));
    };
    assert_eq!(
        expected_dir.acl_manifest_directory_id,
        Some(stage_acl_id),
        "canonical V2 must retain the ACL pointer for src",
    );

    // When: deriving only src from the ACL id already rooted at src.
    let stage_entry = derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        stage_path,
        vec![],
        HashMap::new(),
        file_changes_from_bonsai(&ctx, &repo, commit).await?,
        (None, None),
        &HashMap::new(),
        &Default::default(),
        repo.restricted_paths().config_based(),
        Some(stage_acl_id),
    )
    .await?;

    // Then: the stage entry retains the same ACL pointer as canonical V2.
    assert_eq!(stage_entry, Some(expected_entry));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_entry_writes_restricted_paths_like_canonical(
    fb: FacebookInit,
) -> Result<()> {
    with_just_knobs_async(
        restricted_paths_access_logging_knobs(true),
        async move {
            // Given: equivalent repos with one configured restricted directory.
            let ctx = CoreContext::test_mock(fb);
            let canonical_repo =
                build_repo_with_restricted_path_config(fb, vec![NonRootMPath::new("restricted")?])
                    .await?;
            let staged_repo =
                build_repo_with_restricted_path_config(fb, vec![NonRootMPath::new("restricted")?])
                    .await?;
            let canonical_commit = CreateCommitContext::new_root(&ctx, &canonical_repo)
                .add_file("restricted/secret.txt", "hidden")
                .commit()
                .await?;
            let staged_commit = CreateCommitContext::new_root(&ctx, &staged_repo)
                .add_file("restricted/secret.txt", "hidden")
                .commit()
                .await?;

            // When: one repo derives the canonical root and the other derives only
            // the non-root stage containing the restricted directory.
            let canonical_root = derive_augmented_manifest_directly_for_test(
                &ctx,
                &canonical_repo,
                canonical_commit,
                vec![],
                (None, None),
            )
            .await?;
            let expected_entry =
                lookup_augmented_root_child(&ctx, &canonical_repo, canonical_root, b"restricted")
                    .await?
                    .context("canonical fixture must contain restricted")?;
            let staged_entry =
                derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
                    &ctx,
                    staged_repo.repo_blobstore(),
                    MPath::new("restricted")?,
                    vec![],
                    HashMap::new(),
                    file_changes_from_bonsai(&ctx, &staged_repo, staged_commit).await?,
                    (None, None),
                    &HashMap::new(),
                    &Default::default(),
                    staged_repo.restricted_paths().config_based(),
                    None,
                )
                .await?;
            let canonical_entries =
                hg_augmented_restricted_path_entries(&ctx, &canonical_repo).await?;
            let staged_entries = hg_augmented_restricted_path_entries(&ctx, &staged_repo).await?;

            // Then: the staged entry and its restricted-path row match canonical.
            assert_eq!(staged_entry, Some(expected_entry));
            assert_eq!(staged_entries, canonical_entries);
            assert_eq!(
                staged_entries
                    .iter()
                    .map(RestrictedPathManifestIdEntry::repo_path)
                    .collect::<Result<Vec<_>>>()?,
                vec![RepoPath::dir("restricted")?],
            );

            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_entry_preserves_absent_parent_position(
    fb: FacebookInit,
) -> Result<()> {
    // Given: only p2 has an entry at the non-root stage path.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("p1-only", "p1")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src/p2-only", "p2")
        .commit()
        .await?;
    let p1_aug =
        derive_augmented_manifest_directly_for_test(&ctx, &repo, p1, vec![], (None, None)).await?;
    let p2_aug =
        derive_augmented_manifest_directly_for_test(&ctx, &repo, p2, vec![], (None, None)).await?;
    assert!(
        lookup_augmented_root_child(&ctx, &repo, p1_aug, b"src")
            .await?
            .is_none(),
        "fixture p1 must not contain src",
    );
    let p2_entry = lookup_augmented_root_child(&ctx, &repo, p2_aug, b"src")
        .await?
        .context("fixture p2 must contain src")?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .add_file("src/merged", "merge")
        .commit()
        .await?;
    let canonical_root = derive_augmented_manifest_directly_for_test(
        &ctx,
        &repo,
        merge,
        vec![p1_aug, p2_aug],
        (Some(p1), Some(p2)),
    )
    .await?;
    let expected_entry = lookup_augmented_root_child(&ctx, &repo, canonical_root, b"src")
        .await?
        .context("canonical merge must contain src")?;

    // When: deriving the stage with the absent p1 slot preserved.
    let stage_entry = derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        MPath::new("src")?,
        vec![None, Some(p2_entry)],
        HashMap::new(),
        file_changes_from_bonsai(&ctx, &repo, merge).await?,
        (Some(p1), Some(p2)),
        &HashMap::new(),
        &Default::default(),
        repo.restricted_paths().config_based(),
        None,
    )
    .await?;

    // Then: p2 remains p2 and the staged entry matches canonical direct V2.
    assert_eq!(stage_entry, Some(expected_entry));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_entry_reuses_known_child(fb: FacebookInit) -> Result<()> {
    // Given: a nested child stage has already produced its augmented entry.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("outside", "outside")
        .add_file("src/lib.rs", "lib")
        .add_file("src/generated/output.rs", "generated")
        .commit()
        .await?;
    let file_changes = file_changes_from_bonsai(&ctx, &repo, commit).await?;
    let known_entry = derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        MPath::new("src/generated")?,
        vec![],
        HashMap::new(),
        file_changes.clone(),
        (None, None),
        &HashMap::new(),
        &Default::default(),
        repo.restricted_paths().config_based(),
        None,
    )
    .await?
    .context("generated stage must produce an entry")?;
    let denied_key = match &known_entry {
        HgAugmentedManifestEntry::DirectoryNode(dir) => {
            HgAugmentedManifestId::new(dir.treenode).blobstore_key()
        }
        HgAugmentedManifestEntry::FileNode(_) => {
            return Err(anyhow!("generated stage must produce a directory"));
        }
    };
    let denying_blobstore = DenyGetKeyedBlobstore::new(repo.repo_blobstore().clone(), denied_key);
    let canonical_root =
        derive_augmented_manifest_directly_for_test(&ctx, &repo, commit, vec![], (None, None))
            .await?;
    let expected_entry = lookup_augmented_root_child(&ctx, &repo, canonical_root, b"src")
        .await?
        .context("canonical manifest must contain src")?;

    // When: deriving the parent stage with the nested result supplied as known.
    let stage_entry = derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
        &ctx,
        &denying_blobstore,
        MPath::new("src")?,
        vec![],
        HashMap::from([(MPath::new("src/generated")?, Some(known_entry))]),
        file_changes,
        (None, None),
        &HashMap::new(),
        &Default::default(),
        repo.restricted_paths().config_based(),
        None,
    )
    .await?;

    // Then: the known child is reused without loading it and output stays canonical.
    assert_eq!(stage_entry, Some(expected_entry));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_entry_matches_canonical_file(
    fb: FacebookInit,
) -> Result<()> {
    // Given: the non-root stage path is itself a file.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("outside", "outside")
        .add_file("src", "contents")
        .commit()
        .await?;
    let canonical_root =
        derive_augmented_manifest_directly_for_test(&ctx, &repo, commit, vec![], (None, None))
            .await?;
    let expected_entry = lookup_augmented_root_child(&ctx, &repo, canonical_root, b"src")
        .await?
        .context("canonical manifest must contain src")?;
    assert!(
        matches!(expected_entry, HgAugmentedManifestEntry::FileNode(_)),
        "canonical src entry must be a file",
    );

    // When: deriving directly at the file path.
    let stage_entry = derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        MPath::new("src")?,
        vec![],
        HashMap::new(),
        file_changes_from_bonsai(&ctx, &repo, commit).await?,
        (None, None),
        &HashMap::new(),
        &Default::default(),
        repo.restricted_paths().config_based(),
        None,
    )
    .await?;

    // Then: the staged file entry matches canonical direct V2.
    assert_eq!(stage_entry, Some(expected_entry));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_entry_matches_canonical_absence(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a child commit deletes the file at the non-root stage path.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("outside", "outside")
        .add_file("src", "contents")
        .commit()
        .await?;
    let parent_aug =
        derive_augmented_manifest_directly_for_test(&ctx, &repo, parent, vec![], (None, None))
            .await?;
    let parent_entry = lookup_augmented_root_child(&ctx, &repo, parent_aug, b"src")
        .await?
        .context("parent manifest must contain src")?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .delete_file("src")
        .commit()
        .await?;
    let canonical_root = derive_augmented_manifest_directly_for_test(
        &ctx,
        &repo,
        child,
        vec![parent_aug],
        (Some(parent), None),
    )
    .await?;
    assert!(
        lookup_augmented_root_child(&ctx, &repo, canonical_root, b"src")
            .await?
            .is_none(),
        "canonical child must not contain src",
    );

    // When: deriving the deleted stage from its parent file entry.
    let stage_entry = derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        MPath::new("src")?,
        vec![Some(parent_entry)],
        HashMap::new(),
        file_changes_from_bonsai(&ctx, &repo, child).await?,
        (Some(parent), None),
        &HashMap::new(),
        &Default::default(),
        repo.restricted_paths().config_based(),
        None,
    )
    .await?;

    // Then: the stage reports absence rather than inventing a directory.
    assert_eq!(stage_entry, None);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_entry_skips_copy_lookup_without_copies(
    fb: FacebookInit,
) -> Result<()> {
    // Given: an unchanged directory stage with no copy-from changes.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src/original", "contents")
        .commit()
        .await?;
    let parent_augmented_manifest_id = derive_parent_aug(&ctx, &repo, parent).await?;
    let parent_entry =
        lookup_augmented_root_child(&ctx, &repo, parent_augmented_manifest_id, b"src")
            .await?
            .context("parent manifest must contain src")?;
    let denied_key = match &parent_entry {
        HgAugmentedManifestEntry::DirectoryNode(dir) => {
            HgAugmentedManifestId::new(dir.treenode).blobstore_key()
        }
        HgAugmentedManifestEntry::FileNode(_) => {
            return Err(anyhow!("parent src must be a directory"));
        }
    };
    let denying_blobstore = DenyGetKeyedBlobstore::new(repo.repo_blobstore().clone(), denied_key);

    // When: deriving the unchanged stage while denying loads of its parent envelope.
    let stage_entry = derive_hg_augmented_manifest::derive_augmented_manifest_entry_from_bonsai(
        &ctx,
        &denying_blobstore,
        MPath::new("src")?,
        vec![Some(parent_entry.clone())],
        HashMap::new(),
        vec![],
        (Some(parent), None),
        &HashMap::new(),
        &Default::default(),
        repo.restricted_paths().config_based(),
        None,
    )
    .await?;

    // Then: the parent entry is reused without a copy-source manifest lookup.
    assert_eq!(stage_entry, Some(parent_entry));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_augmented_manifest_matches_existing_path_for_empty_manifest(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a parent commit with one file and a child commit that deletes it.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("obsolete", "contents")
        .commit()
        .await?;
    let parent_augmented_manifest_id = derive_parent_aug(&ctx, &repo, parent).await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .delete_file("obsolete")
        .commit()
        .await?;

    // When: deriving the child directly from Bonsai.
    let direct_id = assert_direct_derive_matches_existing_path(
        &ctx,
        &repo,
        child,
        &[parent_augmented_manifest_id],
        (Some(parent), None),
    )
    .await?;

    // Then: the explicit empty-root path still matches the existing derivation.
    let direct_envelope = direct_id.load(&ctx, repo.repo_blobstore()).await?;
    let entries: Vec<_> = direct_envelope
        .augmented_manifest
        .subentries
        .into_entries(&ctx, repo.repo_blobstore())
        .try_collect()
        .await?;
    assert!(entries.is_empty(), "empty manifest should have no entries");

    Ok(())
}

/// Helper to derive the parent's augmented manifest using the existing path
/// so we have a parent input for the direct-derivation tests.
async fn derive_parent_aug(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<HgAugmentedManifestId> {
    let acl_root_overlay = derive_acl_overlay(ctx, repo, cs_id).await?;
    Ok(
        derive_augmented_manifest_via_existing_path(ctx, repo, cs_id, vec![], acl_root_overlay)
            .await?
            .1,
    )
}

/// Assert that a leaves-only conflict rejected by the existing HgManifest
/// derivation is also rejected by direct augmented-manifest derivation.
///
/// This helper intentionally does not use `assert_dual_derive_agree`: in the
/// disagreement case the legacy path should fail. The direct path is expected
/// to fail in the leaf callback before root finalization matters.
async fn assert_legacy_and_direct_reject_leaf_conflict(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    aug_parents: &[HgAugmentedManifestId],
    bonsai_parents: (Option<ChangesetId>, Option<ChangesetId>),
) -> Result<()> {
    let legacy = repo.derive_hg_changeset(ctx, cs_id).await;
    let legacy_err = legacy.expect_err("legacy HgManifest derivation must reject the conflict");
    let legacy_msg = format!("{legacy_err:#}");
    assert!(
        legacy_msg.contains("Unresolved"),
        "expected legacy unresolved-conflict error, got: {legacy_msg}"
    );

    let file_changes = file_changes_from_bonsai(ctx, repo, cs_id).await?;
    let restricted_paths_config = repo.restricted_paths().config_based();
    let direct = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        ctx,
        repo.repo_blobstore(),
        aug_parents.to_vec(),
        file_changes,
        bonsai_parents,
        &Default::default(),
        restricted_paths_config,
        Default::default(),
    )
    .await;
    assert!(
        direct.is_err(),
        "direct derivation must reject the same leaf conflict as legacy derivation, got: {direct:?}"
    );

    Ok(())
}

/// Octopus merge with three roots that each introduce disjoint top-level
/// files. p1.tree, p2.tree, p3.tree all differ at the root; this is the
/// "ordinary" octopus case that does NOT exercise the positional-indexing
/// regression. It's here as a baseline: if this fails the test scaffolding
/// is broken, not the (X, X, Y) fix.
#[mononoke::fbinit_test]
async fn test_octopus_merge_distinct_parents(fb: FacebookInit) -> Result<()> {
    // Given: three independent parents with disjoint root-level files.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "foo_p1")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("bar", "bar_p2")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("qux", "qux_p3")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;

    // When: creating an octopus merge with no additional file changes.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .commit()
        .await?;

    // Then: the direct derivation agrees with the existing path.
    assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Octopus merge where p1.tree == p2.tree but p3.tree differs. This is the
/// shape that reproduces the regression: with positional indexing, the
/// duplicate (p1, p2) survives `derive_manifest`'s value-only dedup as a
/// single entry and the p3 tree slips into the hg-parents window, producing
/// `HgParents::Two(p1, p3)` instead of the correct `HgParents::One(p1)`.
///
/// We reproduce "p1.tree == p2.tree" by giving p1 and p2 *identical content
/// at every path*. That means at the root, and at every subdirectory, the
/// two HgManifestId values are bit-identical. p3 then introduces a top-level
/// file that breaks the equality at the root.
#[mononoke::fbinit_test]
async fn test_octopus_merge_p1_eq_p2_p3_differs(fb: FacebookInit) -> Result<()> {
    // Given: p1 and p2 have identical trees, while p3 has a distinct tree.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .add_file("extra", "only_in_p3")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;
    assert_eq!(
        aug_p1, aug_p2,
        "Test setup invariant violated: expected p1 and p2 to have identical augmented manifest ids",
    );
    assert_ne!(
        aug_p1, aug_p3,
        "Test setup invariant violated: expected p3 to differ from p1",
    );

    // When: deriving an octopus merge over the (X, X, Y) parents.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .commit()
        .await?;

    // Then: both derivation paths agree despite value-deduped parent entries.
    assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Same regression, but at a non-root path. We want to make sure the fix
/// applies to every tree in the recursion, not just the root.
///
/// Setup: all three parents have the same `dir/inner/file`, so
/// `dir/inner` collapses to identical tree ids in p1 and p2; p3 additionally
/// adds a sibling under `dir/inner`, breaking the tree-id equality at the
/// `dir/inner` level. The merge then reads a value at
/// `dir/inner/different` from p3, forcing `dir/inner` to be re-derived.
#[mononoke::fbinit_test]
async fn test_octopus_merge_p1_eq_p2_p3_differs_subdir(fb: FacebookInit) -> Result<()> {
    // Given: the same (X, X, Y) parent shape appears below the root.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/inner/shared", "same")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/inner/shared", "same")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/inner/shared", "same")
        .add_file("dir/inner/different", "p3_only")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;
    assert_eq!(aug_p1, aug_p2, "p1 and p2 must have identical roots");

    // When: deriving an octopus merge that forces the nested directory to be re-derived.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .commit()
        .await?;

    // Then: both derivation paths agree at the root and throughout the tree.
    assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Octopus merge where (p2, p3) are equal and p1 differs. Symmetric
/// counterpart: with positional indexing the survivor of dedup is
/// `[Traced(0, X), Traced(1, Y)]` (because the dup is between indices 1
/// and 2, not 0 and 1) so this case happens to compute the *correct*
/// (p1, p2) even with the buggy code. We test it anyway as a regression
/// guard against "fixing" the bug by introducing an *opposite* bug.
#[mononoke::fbinit_test]
async fn test_octopus_merge_p2_eq_p3_p1_differs(fb: FacebookInit) -> Result<()> {
    // Given: p2 and p3 have identical trees while p1 differs.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .add_file("extra", "only_in_p1")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;
    assert_eq!(aug_p2, aug_p3);

    // When: deriving the symmetric octopus merge.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .commit()
        .await?;

    // Then: both derivation paths agree, guarding against an opposite-direction bug.
    assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Octopus merge with leaves-only conflict at p3+: a single file is
/// present at all three parents with identical content but with three
/// different filenodes (because of differing ancestry). `derive_manifest`
/// invokes the leaf callback with `change: None`, and the leaf path used
/// to bail with "only supports two-parent merges". The fix routes the
/// resolution through `hg_parents` and reuses the first hg-relevant
/// parent's leaf, matching the existing HgManifest-based path.
#[mononoke::fbinit_test]
async fn test_octopus_merge_leaves_only_conflict(fb: FacebookInit) -> Result<()> {
    // Given: three parents contain the same file content at `foo`, but each has a distinct filenode.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "same_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "same_content")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "same_content")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;

    // When: deriving an octopus merge with no bonsai change for `foo`.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .commit()
        .await?;

    // Then: both paths resolve the leaves-only conflict identically.
    assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Octopus leaves-only conflict where p1 and p2 agree on content, but p3
/// carries different content at the same path. The legacy HgManifest resolver
/// checks every deduped parent before choosing a reusable filenode, so this is
/// unresolved; direct derivation must not silently ignore p3 just because p1
/// and p2 are the only Mercurial filenode parents.
#[mononoke::fbinit_test]
async fn test_octopus_merge_hg_parents_agree_step_parent_disagrees(fb: FacebookInit) -> Result<()> {
    // Given: p1 and p2 carry identical `foo` contents, but p3 disagrees.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "same_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "same_content")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "different_content")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;

    // When: deriving an octopus merge with no bonsai change for `foo`.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .commit()
        .await?;

    // Then: both derivation paths reject the unresolved leaf conflict.
    assert_legacy_and_direct_reject_leaf_conflict(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Octopus leaves-only conflict where only p1 and p3 carry the path, and they
/// disagree on content. This covers the minimal `(p1, p3)` disagreement shape:
/// the direct path must not reuse p1 merely because p2 is absent.
#[mononoke::fbinit_test]
async fn test_octopus_merge_p1_and_step_parent_disagree(fb: FacebookInit) -> Result<()> {
    // Given: p1 carries `foo`, p2 does not, and p3 carries conflicting `foo` content.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "p1_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("bar", "p2_content")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "p3_content")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;

    // When: deriving an octopus merge with no bonsai change for `foo`.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .commit()
        .await?;

    // Then: both derivation paths reject the unresolved leaf conflict.
    assert_legacy_and_direct_reject_leaf_conflict(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Octopus merge that mirrors the existing HgManifest test
/// `derive_hg_manifest test/main.rs::octopus_merges::test_basic_filenode_parents`.
/// Three parents, each contributing one distinct top-level file; the merge
/// modifies all three. This exercises the leaf-side `(p1, p2)` filenode
/// selection (via `hg_parents`) for files that exist only in a single
/// parent: the fix must yield the same filenode parentage as the existing
/// path — in particular `qux` must end up with `(None, None)` because it
/// only existed in p3, which is invisible to Mercurial filenode parentage.
#[mononoke::fbinit_test]
async fn test_octopus_merge_filenode_parents_match_existing_path(fb: FacebookInit) -> Result<()> {
    // Given: three parents each contribute one distinct top-level file.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "foo")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("bar", "bar")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("qux", "qux")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;

    // When: deriving a merge that modifies files from p1, p2, and p3.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .add_file("foo", "foo2")
        .add_file("bar", "bar2")
        .add_file("qux", "qux2")
        .commit()
        .await?;

    // Then: the direct path matches the existing path's filenode-parent choices.
    assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Two-parent merge where the two parents have identical trees. This is
/// the boundary case: `derive_manifest`'s value-only dedup collapses
/// `[Traced(0, X), Traced(1, X)]` to `[Traced(0, X)]`. With both positional
/// and index-based filtering this yields `(p1=X, p2=None)`, so this case
/// happens to be safe under both implementations — we test it as a
/// no-regression guard.
#[mononoke::fbinit_test]
async fn test_two_parent_merge_identical_trees(fb: FacebookInit) -> Result<()> {
    // Given: two parents have identical trees.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    assert_eq!(aug_p1, aug_p2);

    // When: deriving a two-parent merge with no additional changes.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .commit()
        .await?;

    // Then: both paths agree on the boundary dedup case.
    assert_dual_derive_agree(&ctx, &repo, merge, &[aug_p1, aug_p2], (Some(p1), Some(p2))).await?;

    Ok(())
}

/// Octopus merge where a file (`foo`) lives ONLY in the step-parents (p3/p4),
/// with byte-identical content but different filenodes, and is absent from p1
/// and p2. Both derivation paths must agree, and the only way to agree is to
/// mint a fresh parentless filenode for `foo`: Mercurial filenodes encode only
/// (p1, p2) parentage, so a step-parent's filenode — whose linknode is not a
/// Mercurial ancestor of the merge — cannot be reused.
///
/// p3 and p4 reach `foo = "shared"` from different ancestors (a3/a4), so their
/// `foo` filenodes differ even though the final content is identical. That is
/// what makes this a genuine leaves-only conflict: identical filenodes would
/// otherwise dedup to one entry and never reach the leaf callback.
#[mononoke::fbinit_test]
async fn test_octopus_step_parent_only_file_parity(fb: FacebookInit) -> Result<()> {
    assert_step_parent_only_file_parity(fb, "shared").await
}

#[mononoke::fbinit_test]
async fn test_octopus_step_parent_only_escape_header_parity(fb: FacebookInit) -> Result<()> {
    assert_step_parent_only_file_parity(fb, "\x01\nshared").await
}

async fn assert_step_parent_only_file_parity(
    fb: FacebookInit,
    content: &'static str,
) -> Result<()> {
    // Given: p1 and p2 do not contain `foo`, while p3 and p4 contain byte-identical `foo` through different filenodes.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("bar", "bar_p1")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("baz", "baz_p2")
        .commit()
        .await?;
    let a3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "ancestor_three")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new(&ctx, &repo, vec![a3])
        .add_file("foo", content)
        .commit()
        .await?;
    let a4 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("foo", "ancestor_four")
        .commit()
        .await?;
    let p4 = CreateCommitContext::new(&ctx, &repo, vec![a4])
        .add_file("foo", content)
        .commit()
        .await?;
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![a3, a4], None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![p1, p2, p3, p4], None)
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;
    let aug_p4 = derive_parent_aug(&ctx, &repo, p4).await?;
    let foo = MPath::new("foo")?;
    let leaf_p3 = aug_p3
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), foo.clone())
        .await?
        .and_then(Entry::into_leaf)
        .context("p3 must contain foo as a leaf")?;
    let leaf_p4 = aug_p4
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), foo.clone())
        .await?
        .and_then(Entry::into_leaf)
        .context("p4 must contain foo as a leaf")?;
    assert_ne!(
        leaf_p3.filenode, leaf_p4.filenode,
        "test setup: p3 and p4 must carry foo with DIFFERENT filenodes",
    );
    assert_eq!(
        (leaf_p3.content_sha1, leaf_p3.total_size),
        (leaf_p4.content_sha1, leaf_p4.total_size),
        "test setup: p3 and p4 must carry foo with IDENTICAL content",
    );

    // When: deriving a 4-parent octopus merge with no bonsai change for `foo`.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3, p4])
        .commit()
        .await?;

    // Then: both paths agree, which requires minting the same fresh parentless filenode for `foo`.
    let root = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        vec![aug_p1, aug_p2, aug_p3, aug_p4],
        file_changes_from_bonsai(&ctx, &repo, merge).await?,
        (Some(p1), Some(p2)),
        &Default::default(),
        repo.restricted_paths().config_based(),
        derive_acl_overlay(&ctx, &repo, merge).await?,
    )
    .await?;
    let leaf = root
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), foo)
        .await?
        .and_then(Entry::into_leaf)
        .context("merge must contain foo")?;
    let filenode = HgFileNodeId::new(leaf.filenode)
        .load(&ctx, repo.repo_blobstore())
        .await?;
    assert_eq!(
        leaf.file_header_metadata.as_deref().unwrap_or_default(),
        filenode.metadata().as_ref(),
        "augmented metadata must preserve the minted filenode's header"
    );
    assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3, aug_p4],
        (Some(p1), Some(p2)),
    )
    .await?;

    Ok(())
}

/// Octopus merge where a Bonsai delete cancels the step-parent's only
/// distinct contribution, making the merged tree equal to p1.
///
/// This is the reuse case not covered by the previous octopus tests: those
/// leave p3's distinct contribution in the merge, so both paths create fresh
/// envelopes and can agree even with a too-tight reuse guard.
#[mononoke::fbinit_test]
async fn test_octopus_merge_bonsai_delete_cancels_step_parent_subdir(
    fb: FacebookInit,
) -> Result<()> {
    // Given: p1 and p2 have identical `dir/` trees, while p3 differs only by
    // adding `dir/extra`. After parent dedup, the directory parents are the
    // shape `[Traced(0, X_dir), Traced(2, Y_dir)]`: more than one parent is
    // present, but only p1 is hg-relevant.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file", "a")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file", "a")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file", "a")
        .add_file("dir/extra", "b")
        .commit()
        .await?;
    let aug_p1 = derive_parent_aug(&ctx, &repo, p1).await?;
    let aug_p2 = derive_parent_aug(&ctx, &repo, p2).await?;
    let aug_p3 = derive_parent_aug(&ctx, &repo, p3).await?;
    assert_eq!(
        aug_p1, aug_p2,
        "Test setup invariant: p1 and p2 must have identical augmented manifest ids",
    );
    assert_ne!(
        aug_p1, aug_p3,
        "Test setup invariant: p3 must differ from p1",
    );

    // When: the merge deletes `dir/extra`, cancelling p3's distinct
    // contribution and making the merged `dir/` content equal to p1.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .delete_file("dir/extra")
        .commit()
        .await?;

    // Then: direct derivation matches the existing path and reuses p1's root
    // augmented manifest. A guard like `p1.is_some() && p2.is_some()` would
    // skip reuse here and produce a fresh divergent manifest id.
    let derived = assert_dual_derive_agree(
        &ctx,
        &repo,
        merge,
        &[aug_p1, aug_p2, aug_p3],
        (Some(p1), Some(p2)),
    )
    .await?;
    assert_eq!(
        derived, aug_p1,
        "delete-cancelled merge should reuse p1's augmented manifest",
    );

    Ok(())
}

/// Direct Bonsai derivation should use the directly computed root node id
/// rather than a supplied or otherwise non-content-derived one. This is the
/// future path for Bonsai-native commits that have no `MappedHgChangesetId` mapping.
#[mononoke::fbinit_test]
async fn test_direct_derivation_computed_root_when_none(fb: FacebookInit) -> Result<()> {
    // Given: a Bonsai-native root commit with no Hg mapping.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a.txt", "aaa")
        .commit()
        .await?;

    assert!(
        repo.bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, cs_id)
            .await?
            .is_none(),
        "fixture should exercise the no-Hg-mapping root path",
    );

    let file_changes = file_changes_from_bonsai(&ctx, &repo, cs_id).await?;
    let restricted_paths_config = repo.restricted_paths().config_based();

    // When: deriving directly from Bonsai.
    let aug_id_none = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        vec![],
        file_changes,
        (None, None),
        &Default::default(),
        restricted_paths_config,
        None,
    )
    .await?;

    // Then: the root is content-derived and direct derivation did not create an
    // Hg mapping.
    let env = aug_id_none.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        env.augmented_manifest.hg_node_id, env.augmented_manifest.computed_node_id,
        "direct Bonsai root hg_node_id should equal computed_node_id",
    );
    assert!(
        repo.bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, cs_id)
            .await?
            .is_none(),
        "direct derivation with None must not create an Hg mapping",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_derivation_empty_commit_reuses_supplied_parent_root(
    fb: FacebookInit,
) -> Result<()> {
    use mercurial_types::HgNodeHash;
    use mononoke_types::sha1_hash::Sha1 as NodeSha1;

    // Given: an empty commit whose parent's augmented root is stored under a
    // supplied root id, as legacy `unbundle` stores client root hashes.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file", "content")
        .commit()
        .await?;
    let supplied = HgNodeHash::new(NodeSha1::from_byte_array([0xAB; 20]));
    let parent_forced = store_augmented_manifest_with_supplied_root(
        &ctx,
        &repo,
        derive_parent_aug(&ctx, &repo, parent).await?,
        supplied,
    )
    .await?;
    let empty = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .commit()
        .await?;

    // When: deriving the empty commit directly from Bonsai.
    let aug_id = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        vec![parent_forced],
        file_changes_from_bonsai(&ctx, &repo, empty).await?,
        (Some(parent), None),
        &Default::default(),
        repo.restricted_paths().config_based(),
        derive_acl_overlay(&ctx, &repo, empty).await?,
    )
    .await?;

    // Then: like Hg, the empty commit keeps its parent's root id.
    assert_eq!(
        aug_id, parent_forced,
        "an empty commit should reuse the supplied parent root",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_derivation_none_reuses_supplied_parent_envelope(
    fb: FacebookInit,
) -> Result<()> {
    use mercurial_types::HgNodeHash;
    use mononoke_types::sha1_hash::Sha1 as NodeSha1;

    // Given: a no-op merge where p1's augmented root is stored under a supplied
    // root id while p2 has the same content under its content-derived id. This
    // makes `finalize_envelope` consider reusing p1's supplied envelope.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let supplied = HgNodeHash::new(NodeSha1::from_byte_array([0xCD; 20]));
    let restricted_paths_config = repo.restricted_paths().config_based();
    let p1_content_derived = derive_parent_aug(&ctx, &repo, p1).await?;
    let p1_forced =
        store_augmented_manifest_with_supplied_root(&ctx, &repo, p1_content_derived, supplied)
            .await?;
    let p2_content_derived = derive_parent_aug(&ctx, &repo, p2).await?;
    assert_ne!(
        p1_forced, p2_content_derived,
        "the supplied p1 root must differ from p2's content-derived root",
    );
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .commit()
        .await?;
    let file_changes = file_changes_from_bonsai(&ctx, &repo, merge).await?;

    // When: deriving the merge directly from Bonsai.
    let aug_id = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        vec![p1_forced, p2_content_derived],
        file_changes,
        (Some(p1), Some(p2)),
        &Default::default(),
        restricted_paths_config,
        None,
    )
    .await?;

    // Then: like Hg, the direct path reuses p1's supplied envelope.
    let env = aug_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(aug_id, p1_forced, "should reuse the supplied p1 root");
    assert_eq!(env.augmented_manifest.hg_node_id, supplied);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_derivation_none_keeps_supplied_short_circuit_root(
    fb: FacebookInit,
) -> Result<()> {
    use mercurial_types::HgNodeHash;
    use mononoke_types::sha1_hash::Sha1 as NodeSha1;

    // Given: a no-op merge whose parent augmented roots are the same supplied
    // root id and whose merge ACL root is non-empty, so `derive_manifest` can
    // short-circuit and return that parent root without calling
    // `finalize_envelope`.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=restricted\"\n",
        )
        .add_file("restricted/file.rs", "fn restricted() {}")
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=restricted\"\n",
        )
        .add_file("restricted/file.rs", "fn restricted() {}")
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let supplied = HgNodeHash::new(NodeSha1::from_byte_array([0xEF; 20]));
    let restricted_paths_config = repo.restricted_paths().config_based();
    let p1_content_derived = derive_parent_aug(&ctx, &repo, p1).await?;
    let p2_content_derived = derive_parent_aug(&ctx, &repo, p2).await?;
    let p1_forced =
        store_augmented_manifest_with_supplied_root(&ctx, &repo, p1_content_derived, supplied)
            .await?;
    let p2_forced =
        store_augmented_manifest_with_supplied_root(&ctx, &repo, p2_content_derived, supplied)
            .await?;
    assert_eq!(p1_forced, p2_forced, "fixture should force root reuse");
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .commit()
        .await?;
    let acl_root_overlay = derive_acl_overlay(&ctx, &repo, merge)
        .await?
        .context("fixture must have a non-empty ACL overlay")?;

    // When: deriving the merge directly from Bonsai.
    let aug_id = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        &ctx,
        repo.repo_blobstore(),
        vec![p1_forced, p2_forced],
        file_changes_from_bonsai(&ctx, &repo, merge).await?,
        (Some(p1), Some(p2)),
        &Default::default(),
        restricted_paths_config,
        Some(acl_root_overlay),
    )
    .await?;

    // Then: like Hg, the root short-circuit returns the supplied parent root,
    // whose ACL pointer matches the merge root ACL.
    let env = aug_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(aug_id, p1_forced, "should return the supplied parent root");
    assert_eq!(env.augmented_manifest.hg_node_id, supplied);
    assert_eq!(
        env.augmented_manifest.acl_manifest_directory_id,
        Some(acl_root_overlay),
        "the reused root should carry the merge root ACL pointer",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_derivation_keeps_content_derived_short_circuit_root_when_root_acl_pointer_differs(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a no-op merge whose content-derived parent root is visible
    // through an overlay with a stale missing root ACL pointer, while the
    // current merge has a non-empty ACL overlay.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=restricted\"\n",
        )
        .add_file("restricted/file.rs", "fn restricted() {}")
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=restricted\"\n",
        )
        .add_file("restricted/file.rs", "fn restricted() {}")
        .add_file("shared/file", "same_content")
        .commit()
        .await?;
    let p1_content_derived = derive_parent_aug(&ctx, &repo, p1).await?;
    let p2_content_derived = derive_parent_aug(&ctx, &repo, p2).await?;
    assert_eq!(
        p1_content_derived, p2_content_derived,
        "fixture should force derive_manifest to short-circuit to a parent root",
    );

    let stale_blobstore = MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone());
    let mut stale_parent_envelope = p1_content_derived.load(&ctx, repo.repo_blobstore()).await?;
    stale_parent_envelope
        .augmented_manifest
        .acl_manifest_directory_id = None;
    let (augmented_manifest_id, augmented_manifest_size) = stale_parent_envelope
        .augmented_manifest
        .clone()
        .compute_content_addressed_digest(&ctx, &stale_blobstore)
        .await?;
    stale_parent_envelope.augmented_manifest_id = augmented_manifest_id;
    stale_parent_envelope.augmented_manifest_size = augmented_manifest_size;
    let stale_parent_root = stale_parent_envelope.store(&ctx, &stale_blobstore).await?;
    assert_eq!(
        stale_parent_root, p1_content_derived,
        "fixture should shadow the same content-derived root key",
    );

    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .commit()
        .await?;
    let acl_root_overlay = derive_acl_overlay(&ctx, &repo, merge)
        .await?
        .context("fixture must have a non-empty ACL overlay")?;

    // When: deriving the merge directly from Bonsai.
    let aug_id = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        &ctx,
        &stale_blobstore,
        vec![stale_parent_root, p2_content_derived],
        file_changes_from_bonsai(&ctx, &repo, merge).await?,
        (Some(p1), Some(p2)),
        &Default::default(),
        repo.restricted_paths().config_based(),
        Some(acl_root_overlay),
    )
    .await?;

    // Then: like Hg and V1, the merge keeps the parent root id and writes the
    // root under it with the current merge root ACL pointer. The write is
    // visible here only because `MemWritesKeyedBlobstore` overwrites; with the
    // default `IfAbsent` put behaviour the backing store keeps the stale
    // pointer, though caches above it may serve the written one.
    let env = aug_id.load(&ctx, &stale_blobstore).await?;
    assert_eq!(
        aug_id, stale_parent_root,
        "should keep the content-derived parent root id",
    );
    assert_eq!(
        env.augmented_manifest.acl_manifest_directory_id,
        Some(acl_root_overlay),
        "the kept root should be stored with the merge root ACL pointer",
    );

    Ok(())
}

// V1/V2 parity for roots stored under a client-supplied id.
//
// Legacy `unbundle` stores a pushed root tree under the hash the client
// supplied, so that root's `hg_node_id` differs from its `computed_node_id`.
// Wherever Hg reuses such a root, V1 reuses it too, because V1 copies node ids
// from the Hg manifest. These tests derive each commit with both algorithms
// and require identical output.

const SUPPLIED_ROOT_SLACL: &str =
    "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=restricted\"\n";

/// Derives augmented manifests with V1 and V2 into separate in-memory stores
/// over a shared base, giving each side its own earlier outputs as parents.
///
/// V1 builds from the Hg manifest that Mononoke derives for the commit from
/// its parents' Hg changesets; V2 builds from Bonsai. Envelopes are keyed by
/// `hg_node_id`, so in a single store one side's envelope would be dropped
/// wherever the ids collide, and the comparison would read one blob twice.
struct V1V2Parity<B> {
    v1_store: MemWritesKeyedBlobstore<B>,
    v2_store: MemWritesKeyedBlobstore<B>,
    v1_roots: HashMap<ChangesetId, HgAugmentedManifestId>,
    v2_roots: HashMap<ChangesetId, HgAugmentedManifestId>,
}

/// One commit's augmented roots from both algorithms, and its Hg root.
struct V1V2Roots {
    cs_id: ChangesetId,
    hg_root: HgManifestId,
    v1: HgAugmentedManifestId,
    v2: HgAugmentedManifestId,
}

impl<B: KeyedBlobstore + Clone + 'static> V1V2Parity<B> {
    fn new(base: B) -> Self {
        Self {
            v1_store: MemWritesKeyedBlobstore::new(base.clone()),
            v2_store: MemWritesKeyedBlobstore::new(base),
            v1_roots: HashMap::new(),
            v2_roots: HashMap::new(),
        }
    }

    /// Use `root`, already in the base store, as both sides' root for `cs_id`.
    fn add_stored_root(&mut self, cs_id: ChangesetId, root: HgAugmentedManifestId) {
        self.v1_roots.insert(cs_id, root);
        self.v2_roots.insert(cs_id, root);
    }

    async fn derive(
        &mut self,
        ctx: &CoreContext,
        repo: &Repo,
        cs_id: ChangesetId,
    ) -> Result<V1V2Roots> {
        let bonsai: mononoke_types::BonsaiChangeset =
            cs_id.load(ctx, repo.repo_blobstore()).await?;
        let v1_parents = parent_augmented_roots(&self.v1_roots, &bonsai)?;
        let v2_parents = parent_augmented_roots(&self.v2_roots, &bonsai)?;
        let acl_root_overlay = derive_acl_overlay(ctx, repo, cs_id).await?;
        let hg_root = derive_hg_root(ctx, repo, cs_id).await?;
        let restricted_paths = repo.restricted_paths().config_based();
        let v1 = derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
            ctx,
            &self.v1_store,
            hg_root,
            v1_parents,
            &Default::default(),
            restricted_paths,
            acl_root_overlay,
        )
        .await?;
        let v2 = derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai_changeset(
            ctx,
            &self.v2_store,
            &bonsai,
            v2_parents,
            restricted_paths,
            acl_root_overlay,
        )
        .await?;
        self.v1_roots.insert(cs_id, v1);
        self.v2_roots.insert(cs_id, v2);
        Ok(V1V2Roots {
            cs_id,
            hg_root,
            v1,
            v2,
        })
    }

    /// Asserts that V2's output equals V1's: the same root id, which is the
    /// commit's Hg root manifest id, and the same envelope at every directory,
    /// each read from its own side's store. Envelope digests omit ACL
    /// pointers, so equal root ids alone would not show equal ACL pointers.
    async fn assert_v2_matches_v1(
        &self,
        ctx: &CoreContext,
        roots: &V1V2Roots,
        case: &str,
    ) -> Result<()> {
        let cs_id = roots.cs_id;
        assert_eq!(
            roots.v1.into_nodehash(),
            roots.hg_root.into_nodehash(),
            "{case}: V1 root of {cs_id} should be its Hg root manifest",
        );
        assert_eq!(
            roots.v2, roots.v1,
            "{case}: V2 root of {cs_id} should equal V1's"
        );
        assert_eq!(
            load_augmented_directory_envelopes(ctx, &self.v2_store, roots.v2).await?,
            load_augmented_directory_envelopes(ctx, &self.v1_store, roots.v1).await?,
            "{case}: V2 directory envelopes of {cs_id} should equal V1's",
        );
        Ok(())
    }
}

fn parent_augmented_roots(
    roots: &HashMap<ChangesetId, HgAugmentedManifestId>,
    bonsai: &mononoke_types::BonsaiChangeset,
) -> Result<Vec<HgAugmentedManifestId>> {
    bonsai
        .parents()
        .map(|parent| {
            roots
                .get(&parent)
                .copied()
                .ok_or_else(|| anyhow!("no augmented root for parent {parent}"))
        })
        .collect()
}

/// The Hg root manifest that Mononoke derives for `cs_id`. Derives through
/// the derived data manager because `derive_hg_changeset` also derives
/// augmented manifests in tests, which would put V1 output into the base that
/// both sides read.
async fn derive_hg_root(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<HgManifestId> {
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_heads::<MappedHgChangesetId>(ctx.clone(), vec![cs_id], None, None)
        .await?;
    let hg_cs_id = manager
        .fetch_derived::<MappedHgChangesetId>(ctx, cs_id, None)
        .await?
        .with_context(|| format!("missing Hg changeset for {cs_id}"))?
        .hg_changeset_id();
    Ok(hg_cs_id
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid())
}

/// The augmented root that canonical derivation stores for `cs_id`. A commit
/// with an Hg changeset is built from its Hg manifest, so this is V1 output,
/// as in existing history.
async fn derive_stored_root(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<HgAugmentedManifestId> {
    derive_hg_root(ctx, repo, cs_id).await?;
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![cs_id], None, None)
        .await?;
    Ok(manager
        .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
        .await?
        .with_context(|| format!("missing augmented root for {cs_id}"))?
        .hg_augmented_manifest_id())
}

/// `p` adds files to `b`, and its Hg root is stored under a supplied id. Both
/// have stored augmented roots.
struct SuppliedRootHistory {
    b: ChangesetId,
    p: ChangesetId,
    supplied_root: HgAugmentedManifestId,
}

/// Files of the base commit of the supplied-root tests.
fn supplied_root_base_files(with_slacl: bool) -> Vec<(&'static str, &'static str)> {
    let mut files = vec![("dir/a", "a"), ("other/b", "b")];
    if with_slacl {
        files.extend([
            ("restricted/.slacl", SUPPLIED_ROOT_SLACL),
            ("restricted/secret", "secret"),
        ]);
    }
    files
}

async fn create_supplied_root_base(
    ctx: &CoreContext,
    repo: &Repo,
    with_slacl: bool,
) -> Result<ChangesetId> {
    let mut base = CreateCommitContext::new_root(ctx, repo);
    for (path, content) in supplied_root_base_files(with_slacl) {
        base = base.add_file(path, content);
    }
    base.commit().await
}

fn supplied_hg_root_hash() -> mercurial_types::HgNodeHash {
    mercurial_types::HgNodeHash::new(mononoke_types::sha1_hash::Sha1::from_byte_array([0xAB; 20]))
}

async fn create_supplied_root_history(
    ctx: &CoreContext,
    repo: &Repo,
    with_slacl: bool,
) -> Result<SuppliedRootHistory> {
    let b = create_supplied_root_base(ctx, repo, with_slacl).await?;
    let p = CreateCommitContext::new(ctx, repo, vec![b])
        .add_file("dir/p", "p")
        .add_file("new/q", "q")
        .commit()
        .await?;
    let supplied = supplied_hg_root_hash();
    fixtures::store_supplied_hg_root(ctx, repo, p, supplied).await?;

    let supplied_root = derive_stored_root(ctx, repo, p).await?;
    let envelope = supplied_root.load(ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        supplied_root.into_nodehash(),
        supplied,
        "fixture: p's stored root should be its supplied Hg root",
    );
    assert_ne!(
        envelope.augmented_manifest.hg_node_id, envelope.augmented_manifest.computed_node_id,
        "fixture: p's stored root should not be content-derived",
    );
    assert_eq!(
        derive_acl_overlay(ctx, repo, p).await?.is_some(),
        with_slacl,
        "fixture: p should have an ACL root exactly when it has a .slacl file",
    );
    Ok(SuppliedRootHistory {
        b,
        p,
        supplied_root,
    })
}

/// A parity checker whose base holds the stored roots of `history`.
async fn supplied_root_parity(
    ctx: &CoreContext,
    repo: &Repo,
    history: &SuppliedRootHistory,
) -> Result<V1V2Parity<RepoBlobstore>> {
    let mut parity = V1V2Parity::new(repo.repo_blobstore().clone());
    parity.add_stored_root(history.b, derive_stored_root(ctx, repo, history.b).await?);
    parity.add_stored_root(history.p, history.supplied_root);
    Ok(parity)
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_empty_commit_on_supplied_root(fb: FacebookInit) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("empty commit, slacl={with_slacl}");
        // Given: an empty commit whose parent's Hg root is stored under a
        // supplied id.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let empty = CreateCommitContext::new(&ctx, &repo, vec![history.p])
            .commit()
            .await?;

        // When: deriving the empty commit with V1 and V2.
        let roots = parity.derive(&ctx, &repo, empty).await?;

        // Then: V2 matches V1, and both keep the supplied root.
        parity.assert_v2_matches_v1(&ctx, &roots, &case).await?;
        assert_eq!(
            roots.v1, history.supplied_root,
            "{case}: should keep the supplied root"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_descendants_of_reused_supplied_root(
    fb: FacebookInit,
) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("descendants, slacl={with_slacl}");
        // Given: an empty commit on the supplied root, and a child of it that
        // changes a file.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let empty = CreateCommitContext::new(&ctx, &repo, vec![history.p])
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![empty])
            .add_file("dir/a", "a2")
            .commit()
            .await?;

        // When: deriving both commits, each side from its own outputs.
        let empty_roots = parity.derive(&ctx, &repo, empty).await?;
        let child_roots = parity.derive(&ctx, &repo, child).await?;

        // Then: V2 matches V1 for both, and the child's new root has the
        // reused supplied root as p1.
        parity
            .assert_v2_matches_v1(&ctx, &empty_roots, &case)
            .await?;
        parity
            .assert_v2_matches_v1(&ctx, &child_roots, &case)
            .await?;
        assert_eq!(
            empty_roots.v1, history.supplied_root,
            "{case}: the empty commit should keep the supplied root",
        );
        let child_envelope = child_roots.v1.load(&ctx, &parity.v1_store).await?;
        assert_eq!(
            child_envelope.augmented_manifest.p1,
            Some(history.supplied_root.into_nodehash()),
            "{case}: the child's root should have the supplied root as p1",
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_merge_equal_to_supplied_p1(fb: FacebookInit) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("merge equal to p1, slacl={with_slacl}");
        // Given: a merge of the supplied-root commit with its parent, whose
        // files it already has, so the merge result equals p1.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let merge = CreateCommitContext::new(&ctx, &repo, vec![history.p, history.b])
            .commit()
            .await?;

        // When: deriving the merge with V1 and V2.
        let roots = parity.derive(&ctx, &repo, merge).await?;

        // Then: V2 matches V1, and both reuse the supplied p1 root.
        parity.assert_v2_matches_v1(&ctx, &roots, &case).await?;
        assert_eq!(
            roots.v1, history.supplied_root,
            "{case}: should reuse the supplied p1 root"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_merge_equal_to_supplied_p2(fb: FacebookInit) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("merge equal to p2, slacl={with_slacl}");
        // Given: a merge of the supplied-root commit's parent with it, so the
        // merge result equals p2 but not p1.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let merge = CreateCommitContext::new(&ctx, &repo, vec![history.b, history.p])
            .commit()
            .await?;

        // When: deriving the merge with V1 and V2.
        let roots = parity.derive(&ctx, &repo, merge).await?;

        // Then: V2 matches V1, and both reuse the supplied p2 root.
        parity.assert_v2_matches_v1(&ctx, &roots, &case).await?;
        assert_eq!(
            roots.v1, history.supplied_root,
            "{case}: should reuse the supplied p2 root"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_merge_of_parents_sharing_supplied_root(
    fb: FacebookInit,
) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("parents sharing the supplied root, slacl={with_slacl}");
        // Given: two empty children of the supplied-root commit, whose stored
        // roots are both the supplied root, and a merge of the two.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let mut sides = Vec::new();
        for message in ["left", "right"] {
            let side = CreateCommitContext::new(&ctx, &repo, vec![history.p])
                .set_message(message)
                .commit()
                .await?;
            let side_root = derive_stored_root(&ctx, &repo, side).await?;
            assert_eq!(
                side_root, history.supplied_root,
                "fixture: {case}: the {message} parent's stored root should be the supplied root",
            );
            parity.add_stored_root(side, side_root);
            sides.push(side);
        }
        let merge = CreateCommitContext::new(&ctx, &repo, sides)
            .commit()
            .await?;

        // When: deriving the merge with V1 and V2.
        let roots = parity.derive(&ctx, &repo, merge).await?;

        // Then: V2 matches V1, and both keep the shared supplied root.
        parity.assert_v2_matches_v1(&ctx, &roots, &case).await?;
        assert_eq!(
            roots.v1, history.supplied_root,
            "{case}: should keep the shared supplied root",
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_merge_of_parents_with_equal_content(
    fb: FacebookInit,
) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("parents with equal content, slacl={with_slacl}");
        // Given: `q` makes the supplied-root commit's change to the same
        // parent, so it has the same content under a content-derived root, and
        // merges of the two in both orders.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let q = CreateCommitContext::new(&ctx, &repo, vec![history.b])
            .set_message("same change")
            .add_file("dir/p", "p")
            .add_file("new/q", "q")
            .commit()
            .await?;
        let q_root = derive_stored_root(&ctx, &repo, q).await?;
        let supplied_envelope = history
            .supplied_root
            .load(&ctx, repo.repo_blobstore())
            .await?;
        assert_eq!(
            q_root.into_nodehash(),
            supplied_envelope.augmented_manifest.computed_node_id,
            "fixture: {case}: q's root should be the content-derived id of the supplied root",
        );
        parity.add_stored_root(q, q_root);
        let supplied_first = CreateCommitContext::new(&ctx, &repo, vec![history.p, q])
            .commit()
            .await?;
        let derived_first = CreateCommitContext::new(&ctx, &repo, vec![q, history.p])
            .commit()
            .await?;

        // When: deriving both merges with V1 and V2.
        let supplied_first_roots = parity.derive(&ctx, &repo, supplied_first).await?;
        let derived_first_roots = parity.derive(&ctx, &repo, derived_first).await?;

        // Then: V2 matches V1, and like Hg both reuse p1's root.
        parity
            .assert_v2_matches_v1(&ctx, &supplied_first_roots, &case)
            .await?;
        parity
            .assert_v2_matches_v1(&ctx, &derived_first_roots, &case)
            .await?;
        assert_eq!(
            supplied_first_roots.v1, history.supplied_root,
            "{case}: should reuse the supplied p1 root",
        );
        assert_eq!(
            derived_first_roots.v1, q_root,
            "{case}: should reuse the content-derived p1 root",
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_octopus_merge_equal_to_supplied_p1(fb: FacebookInit) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("octopus merge equal to p1, slacl={with_slacl}");
        // Given: `r` adds one of the supplied-root commit's files to the same
        // parent, and an octopus merge of the supplied-root commit, its parent
        // and `r`, whose result equals p1.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let r = CreateCommitContext::new(&ctx, &repo, vec![history.b])
            .set_message("part of the change")
            .add_file("new/q", "q")
            .commit()
            .await?;
        parity.add_stored_root(r, derive_stored_root(&ctx, &repo, r).await?);
        let octopus = CreateCommitContext::new(&ctx, &repo, vec![history.p, history.b, r])
            .commit()
            .await?;

        // When: deriving the octopus merge with V1 and V2.
        let roots = parity.derive(&ctx, &repo, octopus).await?;

        // Then: V2 matches V1, and both reuse the supplied p1 root.
        parity.assert_v2_matches_v1(&ctx, &roots, &case).await?;
        assert_eq!(
            roots.v1, history.supplied_root,
            "{case}: should reuse the supplied p1 root"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_octopus_merge_equal_to_supplied_step_parent(
    fb: FacebookInit,
) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("octopus merge equal to p3, slacl={with_slacl}");
        // Given: an octopus merge whose result equals its third parent, the
        // supplied-root commit. Hg only reuses p1 or p2.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let history = create_supplied_root_history(&ctx, &repo, with_slacl).await?;
        let mut parity = supplied_root_parity(&ctx, &repo, &history).await?;
        let r = CreateCommitContext::new(&ctx, &repo, vec![history.b])
            .set_message("part of the change")
            .add_file("new/q", "q")
            .commit()
            .await?;
        parity.add_stored_root(r, derive_stored_root(&ctx, &repo, r).await?);
        let octopus = CreateCommitContext::new(&ctx, &repo, vec![history.b, r, history.p])
            .commit()
            .await?;

        // When: deriving the octopus merge with V1 and V2.
        let roots = parity.derive(&ctx, &repo, octopus).await?;

        // Then: V2 matches V1, and neither reuses the supplied step-parent
        // root.
        parity.assert_v2_matches_v1(&ctx, &roots, &case).await?;
        assert_ne!(
            roots.v1, history.supplied_root,
            "{case}: should not reuse the supplied p3 root"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_merge_equal_to_empty_supplied_root(fb: FacebookInit) -> Result<()> {
    for with_slacl in [false, true] {
        let case = format!("merge equal to an empty supplied root, slacl={with_slacl}");
        // Given: `cleared` deletes every file of `b`, and its empty Hg root is
        // stored under a supplied id. Merges of the two, in both orders, also
        // delete `b`'s files, so each result is the empty tree.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let base_files = supplied_root_base_files(with_slacl);
        let b = create_supplied_root_base(&ctx, &repo, with_slacl).await?;
        let mut cleared = CreateCommitContext::new(&ctx, &repo, vec![b]);
        for (path, _) in &base_files {
            cleared = cleared.delete_file(*path);
        }
        let cleared = cleared.commit().await?;
        fixtures::store_supplied_hg_root(&ctx, &repo, cleared, supplied_hg_root_hash()).await?;
        let cleared_root = derive_stored_root(&ctx, &repo, cleared).await?;
        let cleared_envelope = cleared_root.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            cleared_root.into_nodehash(),
            supplied_hg_root_hash(),
            "fixture: {case}: the cleared commit's stored root should be its supplied Hg root",
        );
        assert_ne!(
            cleared_envelope.augmented_manifest.hg_node_id,
            cleared_envelope.augmented_manifest.computed_node_id,
            "fixture: {case}: the cleared commit's stored root should not be content-derived",
        );
        let mut parity = V1V2Parity::new(repo.repo_blobstore().clone());
        parity.add_stored_root(b, derive_stored_root(&ctx, &repo, b).await?);
        parity.add_stored_root(cleared, cleared_root);
        let mut merges = Vec::new();
        // Distinct messages: Hg changeset ids ignore parent order, so the two
        // merges would otherwise share one.
        for (message, parents) in [
            ("cleared first", vec![cleared, b]),
            ("base first", vec![b, cleared]),
        ] {
            let mut merge = CreateCommitContext::new(&ctx, &repo, parents).set_message(message);
            for (path, _) in &base_files {
                merge = merge.delete_file(*path);
            }
            merges.push(merge.commit().await?);
        }

        // When: deriving both merges with V1 and V2.
        let mut merge_roots = Vec::new();
        for merge in merges {
            merge_roots.push(parity.derive(&ctx, &repo, merge).await?);
        }

        // Then: V2 matches V1, and both reuse the empty supplied root, as p1
        // in one merge and as p2 in the other.
        for roots in &merge_roots {
            parity.assert_v2_matches_v1(&ctx, roots, &case).await?;
            assert_eq!(
                roots.v1, cleared_root,
                "{case}: should reuse the empty supplied root",
            );
        }
    }
    Ok(())
}

/// A reusable root whose stored ACL pointer disagrees with the commit's ACL
/// root. V1 keeps the Hg root id and writes the root under it with the
/// commit's pointer.
#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_empty_commit_on_supplied_root_with_stale_acl_pointer(
    fb: FacebookInit,
) -> Result<()> {
    // Given: an empty commit on the supplied-root commit, whose stored root has
    // no ACL pointer although the repo has an ACL root, as for an envelope
    // stored before ACL pointers existed.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let history = create_supplied_root_history(&ctx, &repo, true).await?;
    let stale_base = MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone());
    let mut stale_envelope = history
        .supplied_root
        .load(&ctx, repo.repo_blobstore())
        .await?;
    stale_envelope.augmented_manifest.acl_manifest_directory_id = None;
    assert_eq!(
        stale_envelope.store(&ctx, &stale_base).await?,
        history.supplied_root,
        "fixture: the stale envelope should be stored under the supplied root",
    );
    let mut parity = V1V2Parity::new(stale_base);
    parity.add_stored_root(history.b, derive_stored_root(&ctx, &repo, history.b).await?);
    parity.add_stored_root(history.p, history.supplied_root);
    let empty = CreateCommitContext::new(&ctx, &repo, vec![history.p])
        .commit()
        .await?;
    let acl_root_overlay = derive_acl_overlay(&ctx, &repo, empty)
        .await?
        .context("fixture: the empty commit should have an ACL root")?;

    // When: deriving the empty commit with V1 and V2.
    let roots = parity.derive(&ctx, &repo, empty).await?;

    // Then: V2 matches V1, and both keep the supplied root and write it with
    // the commit's ACL root. The write is visible here only because the parity
    // stores overwrite; with the default `IfAbsent` put behaviour the backing
    // store keeps the stale pointer, though caches above it may serve the
    // written one.
    parity
        .assert_v2_matches_v1(&ctx, &roots, "empty commit on a stale supplied root")
        .await?;
    assert_eq!(
        roots.v1, history.supplied_root,
        "should keep the supplied root"
    );
    let v2_envelope = roots.v2.load(&ctx, &parity.v2_store).await?;
    assert_eq!(
        v2_envelope.augmented_manifest.acl_manifest_directory_id,
        Some(acl_root_overlay),
        "the kept root should carry the commit's ACL root",
    );
    Ok(())
}

/// A merged directory equal to p1's, whose stored envelope has no ACL pointer
/// although p1's root records one for it. V1 copies p1's entry for the
/// directory and does not rewrite the directory.
#[mononoke::fbinit_test]
async fn test_v2_matches_v1_for_merge_reusing_directory_with_stale_acl_pointer(
    fb: FacebookInit,
) -> Result<()> {
    // Given: `p1` adds a file under the restriction root `restricted/`, `p2`
    // adds one elsewhere, and their merge keeps p1's `restricted/`. That
    // directory's envelope is stored without an ACL pointer, as before ACL
    // pointers existed, while p1's root still records the pointer for it.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let b = create_supplied_root_base(&ctx, &repo, true).await?;
    let p1 = CreateCommitContext::new(&ctx, &repo, vec![b])
        .add_file("restricted/extra", "extra")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new(&ctx, &repo, vec![b])
        .add_file("other/c", "c")
        .commit()
        .await?;
    let p1_root = derive_stored_root(&ctx, &repo, p1).await?;
    let p1_restricted_entry = lookup_augmented_root_child(&ctx, &repo, p1_root, b"restricted")
        .await?
        .context("fixture: p1 should contain restricted/")?;
    let HgAugmentedManifestEntry::DirectoryNode(p1_restricted) = &p1_restricted_entry else {
        return Err(anyhow!("fixture: p1's restricted/ should be a directory"));
    };
    assert!(
        p1_restricted.acl_manifest_directory_id.is_some(),
        "fixture: p1's root should record an ACL pointer for restricted/",
    );
    let p1_restricted_id = HgAugmentedManifestId::new(p1_restricted.treenode);
    let stale_base = MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone());
    let mut stale_envelope = p1_restricted_id.load(&ctx, repo.repo_blobstore()).await?;
    stale_envelope.augmented_manifest.acl_manifest_directory_id = None;
    assert_eq!(
        stale_envelope.store(&ctx, &stale_base).await?,
        p1_restricted_id,
        "fixture: the stale envelope should be stored under p1's restricted/ id",
    );
    let mut parity = V1V2Parity::new(stale_base);
    parity.add_stored_root(p1, p1_root);
    parity.add_stored_root(p2, derive_stored_root(&ctx, &repo, p2).await?);
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .commit()
        .await?;

    // When: deriving the merge with V1 and V2.
    let roots = parity.derive(&ctx, &repo, merge).await?;

    // Then: V2 matches V1, and both keep p1's entry for restricted/.
    parity
        .assert_v2_matches_v1(&ctx, &roots, "merge reusing a stale restricted/")
        .await?;
    let merge_restricted_entry = roots
        .v1
        .load(&ctx, &parity.v1_store)
        .await?
        .augmented_manifest
        .lookup(
            &ctx,
            &parity.v1_store,
            &MPathElement::new_from_slice(b"restricted")?,
        )
        .await?;
    assert_eq!(
        merge_restricted_entry,
        Some(p1_restricted_entry),
        "the merge should keep p1's entry for restricted/",
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_direct_derivation_tracks_restricted_paths_like_existing_path(
    fb: FacebookInit,
) -> Result<()> {
    with_just_knobs_async(
        restricted_paths_access_logging_knobs(true),
        async move {
            // Given: two equivalent repos with a two-commit stack, two configured
            // restriction roots, and an unrestricted public directory.
            let ctx = CoreContext::test_mock(fb);
            let restricted_paths = vec![
                NonRootMPath::new("restricted")?,
                NonRootMPath::new("restricted/nested")?,
            ];
            let existing_repo =
                build_repo_with_restricted_path_config(fb, restricted_paths.clone()).await?;
            let direct_repo = build_repo_with_restricted_path_config(fb, restricted_paths).await?;
            let (existing_parent, existing_child) =
                create_restricted_path_test_stack(&ctx, &existing_repo).await?;
            let (direct_parent, direct_child) =
                create_restricted_path_test_stack(&ctx, &direct_repo).await?;

            // When: deriving one repo through the existing HgManifest-based path and
            // the other through the direct Bonsai-based path.
            let (_, existing_parent_augmented) = derive_augmented_manifest_via_existing_path(
                &ctx,
                &existing_repo,
                existing_parent,
                vec![],
                None,
            )
            .await?;
            let (_, existing_child_augmented) = derive_augmented_manifest_via_existing_path(
                &ctx,
                &existing_repo,
                existing_child,
                vec![existing_parent_augmented],
                None,
            )
            .await?;
            let direct_parent_augmented = derive_augmented_manifest_directly_for_test(
                &ctx,
                &direct_repo,
                direct_parent,
                vec![],
                (None, None),
            )
            .await?;
            let direct_child_augmented = derive_augmented_manifest_directly_for_test(
                &ctx,
                &direct_repo,
                direct_child,
                vec![direct_parent_augmented],
                (Some(direct_parent), None),
            )
            .await?;
            let existing_entries =
                hg_augmented_restricted_path_entries(&ctx, &existing_repo).await?;
            let direct_entries = hg_augmented_restricted_path_entries(&ctx, &direct_repo).await?;

            // Then: the direct path writes the same HgAugmented manifest-id entries as
            // the existing path, and only for configured restricted directory roots.
            assert_eq!(direct_parent_augmented, existing_parent_augmented);
            assert_eq!(direct_child_augmented, existing_child_augmented);
            assert_eq!(direct_entries, existing_entries);
            assert_eq!(
                direct_entries.len(),
                3,
                "expected parent restricted/ plus child restricted/ and restricted/nested entries, got: {direct_entries:?}",
            );
            let entry_paths: Vec<_> = direct_entries
                .iter()
                .map(|entry| entry.repo_path())
                .collect::<Result<_>>()?;
            assert!(entry_paths.contains(&RepoPath::dir("restricted")?));
            assert!(entry_paths.contains(&RepoPath::dir("restricted/nested")?));
            assert!(!entry_paths.contains(&RepoPath::dir("public")?));

            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn test_direct_derivation_does_not_track_restricted_paths_when_jk_disabled(
    fb: FacebookInit,
) -> Result<()> {
    with_just_knobs_async(
        restricted_paths_access_logging_knobs(false),
        async move {
            // Given: a repo with configured restriction roots but the access-logging
            // JK disabled.
            let ctx = CoreContext::test_mock(fb);
            let repo = build_repo_with_restricted_path_config(
                fb,
                vec![
                    NonRootMPath::new("restricted")?,
                    NonRootMPath::new("restricted/nested")?,
                ],
            )
            .await?;
            let (parent, child) = create_restricted_path_test_stack(&ctx, &repo).await?;

            // When: deriving the same two-commit stack through the direct path.
            let parent_augmented = derive_augmented_manifest_directly_for_test(
                &ctx,
                &repo,
                parent,
                vec![],
                (None, None),
            )
            .await?;
            derive_augmented_manifest_directly_for_test(
                &ctx,
                &repo,
                child,
                vec![parent_augmented],
                (Some(parent), None),
            )
            .await?;

            // Then: no HgAugmented restricted-path entries are written.
            let entries = hg_augmented_restricted_path_entries(&ctx, &repo).await?;
            assert_eq!(
                entries,
                Vec::<RestrictedPathManifestIdEntry>::new(),
                "restricted-path tracking should be gated entirely by the JK",
            );

            Ok(())
        }
        .boxed(),
    )
    .await
}

/// Recursively compare ACL pointers between two augmented manifests, reading
/// each side from a distinct blobstore so the comparison is genuinely
/// old-vs-new rather than self-vs-self.
///
/// The augmented manifest id IS the Hg node hash (see
/// `HgAugmentedManifestEnvelope::store`), so old and new envelopes share the
/// same blobstore key. To compare their ACL pointer fields we need to load
/// each side from a blobstore that holds ITS write. Callers therefore route
/// each path's writes to its own `MemWritesKeyedBlobstore` overlay and pass
/// the two overlays here; each overlay serves its own write on a `load(...)`
/// of the shared key, and only falls through to the main blobstore for
/// unrelated content (HgManifest blobs, ACL manifest blobs, content blobs).
///
/// Checks the envelope's `acl_manifest_directory_id` AND each directory
/// subentry's `acl_manifest_directory_id` at every level. Also asserts
/// subentry count and per-position kind match, so a missing entry or a
/// directory/leaf swap fails loudly instead of being silently skipped.
async fn compare_augmented_manifests_acl_recursive(
    ctx: &CoreContext,
    old_blobstore: &(impl blobstore::KeyedBlobstore + 'static),
    new_blobstore: &(impl blobstore::KeyedBlobstore + 'static),
    old_id: HgAugmentedManifestId,
    new_id: HgAugmentedManifestId,
    path: mononoke_types::MPath,
) -> Result<()> {
    use mercurial_types::HgAugmentedManifestEntry;

    let old_env = old_id.load(ctx, old_blobstore).await?;
    let new_env = new_id.load(ctx, new_blobstore).await?;

    assert_eq!(
        old_env.augmented_manifest.acl_manifest_directory_id,
        new_env.augmented_manifest.acl_manifest_directory_id,
        "ACL pointer mismatch at path {path:?}",
    );

    let old_entries: Vec<_> = old_env
        .augmented_manifest
        .into_subentries(ctx, old_blobstore)
        .try_collect()
        .await?;
    let new_entries: Vec<_> = new_env
        .augmented_manifest
        .into_subentries(ctx, new_blobstore)
        .try_collect()
        .await?;

    assert_eq!(
        old_entries.len(),
        new_entries.len(),
        "Subentry count mismatch at path {path:?}: old={}, new={}",
        old_entries.len(),
        new_entries.len(),
    );

    for ((old_name, old_entry), (new_name, new_entry)) in old_entries.iter().zip(new_entries.iter())
    {
        assert_eq!(old_name, new_name, "Subentry name mismatch at {path:?}");
        match (old_entry, new_entry) {
            (
                HgAugmentedManifestEntry::DirectoryNode(old_dir),
                HgAugmentedManifestEntry::DirectoryNode(new_dir),
            ) => {
                assert_eq!(
                    old_dir.acl_manifest_directory_id, new_dir.acl_manifest_directory_id,
                    "Child ACL pointer mismatch for {old_name:?} at {path:?}",
                );
                let child_path = path.join(std::iter::once(old_name));
                Box::pin(compare_augmented_manifests_acl_recursive(
                    ctx,
                    old_blobstore,
                    new_blobstore,
                    HgAugmentedManifestId::new(old_dir.treenode),
                    HgAugmentedManifestId::new(new_dir.treenode),
                    child_path,
                ))
                .await?;
            }
            (HgAugmentedManifestEntry::FileNode(_), HgAugmentedManifestEntry::FileNode(_)) => {
                // File leaves carry no ACL pointer; nothing to check here.
            }
            _ => panic!(
                "Entry kind mismatch for {old_name:?} at {path:?}: old={old_entry:?} new={new_entry:?}",
            ),
        }
    }

    Ok(())
}

/// Derive `cs_id` via the direct Bonsai path into `overlay`. Uses the
/// RootAclManifestId overlay. Used by the ACL parity test alongside
/// `derive_existing_into_overlay` to keep each path's envelope
/// writes in its own blobstore.
async fn derive_into_overlay<B>(
    ctx: &CoreContext,
    repo: &Repo,
    overlay: &B,
    cs_id: ChangesetId,
    aug_parents: Vec<HgAugmentedManifestId>,
    bonsai_parents: (Option<ChangesetId>, Option<ChangesetId>),
) -> Result<HgAugmentedManifestId>
where
    B: blobstore::KeyedBlobstore + Clone + 'static,
{
    let acl_root_overlay = derive_acl_overlay(ctx, repo, cs_id).await?;
    let file_changes = file_changes_from_bonsai(ctx, repo, cs_id).await?;
    derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai(
        ctx,
        overlay,
        aug_parents,
        file_changes,
        bonsai_parents,
        &Default::default(),
        repo.restricted_paths().config_based(),
        acl_root_overlay,
    )
    .await
}

/// Derive `cs_id` via the existing HgManifest-based path into `overlay`.
/// Mirror of `derive_into_overlay` for the existing path. Used by the ACL
/// parity test to keep the existing path's writes isolated from the direct
/// path's so the comparison is genuinely existing-vs-direct (and doesn't
/// rely on `PutBehaviour::IfAbsent` in the shared main blobstore, which
/// would silently drop one side's writes on key collision).
async fn derive_existing_into_overlay<B>(
    ctx: &CoreContext,
    repo: &Repo,
    overlay: &B,
    cs_id: ChangesetId,
    aug_parents: Vec<HgAugmentedManifestId>,
) -> Result<HgAugmentedManifestId>
where
    B: blobstore::KeyedBlobstore + Clone + 'static,
{
    let acl_root_overlay = derive_acl_overlay(ctx, repo, cs_id).await?;
    let hg_manifest_id = repo
        .derive_hg_changeset(ctx, cs_id)
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid();
    derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        overlay,
        hg_manifest_id,
        aug_parents,
        &Default::default(),
        repo.restricted_paths().config_based(),
        acl_root_overlay,
    )
    .await
}

async fn derive_acl_parity_pair<OldStore, NewStore>(
    ctx: &CoreContext,
    repo: &Repo,
    old_blobstore: &OldStore,
    new_blobstore: &NewStore,
    cs_id: ChangesetId,
    old_parents: Vec<HgAugmentedManifestId>,
    new_parents: Vec<HgAugmentedManifestId>,
    bonsai_parents: (Option<ChangesetId>, Option<ChangesetId>),
) -> Result<(HgAugmentedManifestId, HgAugmentedManifestId)>
where
    OldStore: blobstore::KeyedBlobstore + Clone + 'static,
    NewStore: blobstore::KeyedBlobstore + Clone + 'static,
{
    let old_id = derive_existing_into_overlay(ctx, repo, old_blobstore, cs_id, old_parents).await?;
    let new_id =
        derive_into_overlay(ctx, repo, new_blobstore, cs_id, new_parents, bonsai_parents).await?;
    Ok((old_id, new_id))
}

async fn assert_augmented_manifest_acl_parity<OldStore, NewStore>(
    ctx: &CoreContext,
    old_blobstore: &OldStore,
    new_blobstore: &NewStore,
    old_id: HgAugmentedManifestId,
    new_id: HgAugmentedManifestId,
    case_name: &str,
) -> Result<()>
where
    OldStore: blobstore::KeyedBlobstore + 'static,
    NewStore: blobstore::KeyedBlobstore + 'static,
{
    assert_eq!(new_id, old_id, "{case_name}: augmented manifest id parity");
    compare_augmented_manifests_acl_recursive(
        ctx,
        old_blobstore,
        new_blobstore,
        old_id,
        new_id,
        mononoke_types::MPath::ROOT,
    )
    .await
}

/// Root commits with nested restriction roots should match the existing path's
/// ACL pointers at every directory envelope and directory subentry.
#[mononoke::fbinit_test]
async fn test_direct_derivation_matches_existing_acl_pointers_for_root_commit(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a root commit with a top-level restriction root and a nested one.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let old_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/code/secret.rs", "fn secret() {}")
        .add_file(
            "restricted/code/inner/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project2\"\n",
        )
        .add_file("restricted/code/inner/deep.rs", "fn deep() {}")
        .add_file("public/readme.md", "hello")
        .commit()
        .await?;

    // When: deriving that root commit through both augmented-manifest paths.
    let (old_root, new_root) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        root,
        vec![],
        vec![],
        (None, None),
    )
    .await?;

    // Then: the direct path matches the existing path, including recursive ACL pointers.
    assert_augmented_manifest_acl_parity(
        &ctx,
        &*old_blobstore,
        &*new_blobstore,
        old_root,
        new_root,
        "root commit with nested restrictions",
    )
    .await
}

/// Single-parent file additions under an existing restricted subtree should
/// preserve the existing path's ACL pointers.
#[mononoke::fbinit_test]
async fn test_direct_derivation_matches_existing_acl_pointers_for_restricted_file_add(
    fb: FacebookInit,
) -> Result<()> {
    // Given: an already-derived restricted parent and a child that only adds a file inside it.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let old_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/code/secret.rs", "fn secret() {}")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("restricted/code/more.rs", "fn more() {}")
        .commit()
        .await?;
    let (old_root, new_root) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        root,
        vec![],
        vec![],
        (None, None),
    )
    .await?;

    // When: deriving the child commit through both augmented-manifest paths.
    let (old_child, new_child) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        child,
        vec![old_root],
        vec![new_root],
        (Some(root), None),
    )
    .await?;

    // Then: the direct path matches the existing path, including recursive ACL pointers.
    assert_augmented_manifest_acl_parity(
        &ctx,
        &*old_blobstore,
        &*new_blobstore,
        old_child,
        new_child,
        "single-parent file add under restricted subtree",
    )
    .await
}

/// Deleting a `.slacl` file should make the direct path drop ACL pointers in
/// the same places as the existing path.
#[mononoke::fbinit_test]
async fn test_direct_derivation_matches_existing_acl_pointers_for_slacl_delete(
    fb: FacebookInit,
) -> Result<()> {
    // Given: an already-derived chain where the target commit deletes a nested `.slacl`.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let old_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file(
            "restricted/code/inner/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project2\"\n",
        )
        .add_file("restricted/code/inner/deep.rs", "fn deep() {}")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("restricted/code/more.rs", "fn more() {}")
        .commit()
        .await?;
    let target = CreateCommitContext::new(&ctx, &repo, vec![child])
        .delete_file("restricted/code/inner/.slacl")
        .commit()
        .await?;
    let (old_root, new_root) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        root,
        vec![],
        vec![],
        (None, None),
    )
    .await?;
    let (old_child, new_child) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        child,
        vec![old_root],
        vec![new_root],
        (Some(root), None),
    )
    .await?;

    // When: deriving the `.slacl` deletion commit through both augmented-manifest paths.
    let (old_target, new_target) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        target,
        vec![old_child],
        vec![new_child],
        (Some(child), None),
    )
    .await?;

    // Then: the direct path matches the existing path, including recursive ACL pointers.
    assert_augmented_manifest_acl_parity(
        &ctx,
        &*old_blobstore,
        &*new_blobstore,
        old_target,
        new_target,
        "nested .slacl deletion",
    )
    .await
}

/// Reused sibling subtrees should keep their parent-stored ACL pointer while a
/// different sibling's `.slacl` changes.
#[mononoke::fbinit_test]
async fn test_direct_derivation_preserves_acl_pointer_for_reused_sibling_subtree(
    fb: FacebookInit,
) -> Result<()> {
    // Given: two sibling restriction roots and a child that deletes only one sibling's `.slacl`.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let old_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=top\"\n",
        )
        .add_file(
            "restricted/keep/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=keep\"\n",
        )
        .add_file("restricted/keep/file.rs", "fn keep() {}")
        .add_file(
            "restricted/other/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=other\"\n",
        )
        .add_file("restricted/other/file.rs", "fn other() {}")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("restricted/other/.slacl")
        .commit()
        .await?;
    let (old_root, new_root) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        root,
        vec![],
        vec![],
        (None, None),
    )
    .await?;

    // When: deriving the sibling `.slacl` deletion commit through both paths.
    let (old_child, new_child) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        child,
        vec![old_root],
        vec![new_root],
        (Some(root), None),
    )
    .await?;

    // Then: the reused sibling subtree's ACL pointer still matches the existing path.
    assert_augmented_manifest_acl_parity(
        &ctx,
        &*old_blobstore,
        &*new_blobstore,
        old_child,
        new_child,
        "reused sibling subtree across .slacl deletion",
    )
    .await
}

/// Clean two-parent merges with ACL roots on both branches should match the
/// existing path, including reused subtrees from the common base.
#[mononoke::fbinit_test]
async fn test_direct_derivation_matches_existing_acl_pointers_for_clean_acl_merge(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a merge whose two branches add independent restriction roots.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let old_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "shared/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=shared\"\n",
        )
        .add_file("shared/file.rs", "fn shared() {}")
        .commit()
        .await?;
    let branch_a = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file(
            "branch_a/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_a\"\n",
        )
        .add_file("branch_a/a.rs", "fn a() {}")
        .commit()
        .await?;
    let branch_b = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file(
            "branch_b/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project_b\"\n",
        )
        .add_file("branch_b/b.rs", "fn b() {}")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![branch_a, branch_b])
        .commit()
        .await?;
    let (old_base, new_base) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        base,
        vec![],
        vec![],
        (None, None),
    )
    .await?;
    let (old_a, new_a) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        branch_a,
        vec![old_base],
        vec![new_base],
        (Some(base), None),
    )
    .await?;
    let (old_b, new_b) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        branch_b,
        vec![old_base],
        vec![new_base],
        (Some(base), None),
    )
    .await?;

    // When: deriving the merge commit through both augmented-manifest paths.
    let (old_merge, new_merge) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        merge,
        vec![old_a, old_b],
        vec![new_a, new_b],
        (Some(branch_a), Some(branch_b)),
    )
    .await?;

    // Then: the direct path matches the existing path, including recursive ACL pointers.
    assert_augmented_manifest_acl_parity(
        &ctx,
        &*old_blobstore,
        &*new_blobstore,
        old_merge,
        new_merge,
        "clean ACL merge",
    )
    .await
}

/// Merges can rebuild restricted directories that are not reachable from
/// `file_changes`; this locks ACL pointer parity for callback-local rebuilt
/// directory lookup.
#[mononoke::fbinit_test]
async fn test_direct_derivation_matches_existing_acl_pointers_for_merge_divergent_dir(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a merge where parent trees diverge under `proj/`, but the merge's
    // changed-file frontier only includes an unrelated root file.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let old_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "proj/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=proj\"\n",
        )
        .add_file("proj/common.rs", "fn common() {}")
        .commit()
        .await?;
    let branch_a = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("proj/extra.rs", "fn extra() {}")
        .commit()
        .await?;
    let branch_b = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("unrelated.rs", "fn unrelated() {}")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![branch_a, branch_b])
        .commit()
        .await?;
    let (old_base, new_base) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        base,
        vec![],
        vec![],
        (None, None),
    )
    .await?;
    let (old_a, new_a) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        branch_a,
        vec![old_base],
        vec![new_base],
        (Some(base), None),
    )
    .await?;
    let (old_b, new_b) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        branch_b,
        vec![old_base],
        vec![new_base],
        (Some(base), None),
    )
    .await?;

    // When: deriving the merge commit through both augmented-manifest paths.
    let (old_merge, new_merge) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &*new_blobstore,
        merge,
        vec![old_a, old_b],
        vec![new_a, new_b],
        (Some(branch_a), Some(branch_b)),
    )
    .await?;

    // Then: the direct path's merge fallback preserves `proj/`'s ACL pointer.
    assert_augmented_manifest_acl_parity(
        &ctx,
        &*old_blobstore,
        &*new_blobstore,
        old_merge,
        new_merge,
        "merge rebuild of parent-divergent restricted dir",
    )
    .await
}

#[mononoke::fbinit_test]
async fn test_direct_merge_acl_lookup_does_not_load_unrelated_acl_subtree(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a merge rebuilds `proj/`, while an unrelated ACL subtree exists
    // elsewhere and is not rebuilt by the merge.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let old_blobstore = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_blobstore = MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone());
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "proj/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=proj\"\n",
        )
        .add_file("proj/common.rs", "fn common() {}")
        .add_file(
            "unrelated/deep/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=unrelated\"\n",
        )
        .add_file("unrelated/deep/secret.rs", "fn secret() {}")
        .commit()
        .await?;
    let branch_a = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("proj/extra.rs", "fn extra() {}")
        .commit()
        .await?;
    let branch_b = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("other.rs", "fn other() {}")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![branch_a, branch_b])
        .commit()
        .await?;

    let (old_base, new_base) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &new_blobstore,
        base,
        vec![],
        vec![],
        (None, None),
    )
    .await?;
    let (old_a, new_a) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &new_blobstore,
        branch_a,
        vec![old_base],
        vec![new_base],
        (Some(base), None),
    )
    .await?;
    let (old_b, new_b) = derive_acl_parity_pair(
        &ctx,
        &repo,
        &*old_blobstore,
        &new_blobstore,
        branch_b,
        vec![old_base],
        vec![new_base],
        (Some(base), None),
    )
    .await?;
    let old_merge =
        derive_existing_into_overlay(&ctx, &repo, &*old_blobstore, merge, vec![old_a, old_b])
            .await?;

    let acl_root_overlay = derive_acl_overlay(&ctx, &repo, merge)
        .await?
        .context("fixture must have a non-empty ACL overlay")?;
    let root_only_acl_map = derive_hg_augmented_manifest::targeted_acl_overlay_map(
        &ctx,
        repo.repo_blobstore(),
        &MPath::ROOT,
        acl_root_overlay,
        &[MPath::ROOT].into_iter().collect(),
    )
    .await?;
    let unrelated_path = MPath::from(NonRootMPath::new("unrelated")?);
    let denied_key = root_only_acl_map
        .get(&unrelated_path)
        .copied()
        .context("fixture must expose an unrelated ACL child pointer")?
        .blobstore_key();
    let denying_blobstore = DenyGetKeyedBlobstore::new(new_blobstore.clone(), denied_key.clone());

    // When: deriving the merge through the direct path while denying loads of
    // the unrelated ACL subtree.
    let new_merge = derive_into_overlay(
        &ctx,
        &repo,
        &denying_blobstore,
        merge,
        vec![new_a, new_b],
        (Some(branch_a), Some(branch_b)),
    )
    .await
    .with_context(|| {
        format!("direct merge derivation should not load unrelated ACL key {denied_key}")
    })?;

    // Then: direct derivation succeeds and still matches the HgManifest-based
    // augmented manifest, proving the unrelated ACL subtree was not needed.
    assert_augmented_manifest_acl_parity(
        &ctx,
        &*old_blobstore,
        &denying_blobstore,
        old_merge,
        new_merge,
        "merge ACL lookup without unrelated subtree load",
    )
    .await
}

/// `targeted_acl_overlay_map` should load the touched ACL branch and immediate
/// sibling pointers, but not descend into untouched sibling subtrees.
#[mononoke::fbinit_test]
async fn test_targeted_acl_overlay_map_is_scoped_to_changed_frontier(
    fb: FacebookInit,
) -> Result<()> {
    // Given: two sibling ACL subtrees and a target frontier under only one sibling.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "a/inner/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=a\"\n",
        )
        .add_file("a/inner/x.rs", "fn x() {}")
        .add_file(
            "b/inner/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=b\"\n",
        )
        .add_file("b/inner/y.rs", "fn y() {}")
        .commit()
        .await?;
    let root_acl_id = derive_acl_overlay(&ctx, &repo, cs_id)
        .await?
        .expect("repo has .slacl files, so the overlay root must be Some");
    let dir = |s: &str| -> Result<MPath> { Ok(MPath::from(NonRootMPath::new(s)?)) };
    let target_dirs: HashSet<MPath> = [MPath::ROOT, dir("a")?, dir("a/inner")?]
        .into_iter()
        .collect();

    // When: loading the targeted ACL overlay map for that frontier.
    let map = derive_hg_augmented_manifest::targeted_acl_overlay_map(
        &ctx,
        repo.repo_blobstore(),
        &MPath::ROOT,
        root_acl_id,
        &target_dirs,
    )
    .await?;

    // Then: the touched branch and immediate sibling pointer are present, but
    // the untouched sibling subtree is not loaded.
    assert!(map.contains_key(&MPath::ROOT), "root pointer recorded");
    assert!(map.contains_key(&dir("a")?), "a/ descended");
    assert!(
        map.contains_key(&dir("a/inner")?),
        "a/inner restriction root recorded"
    );
    assert!(
        map.contains_key(&dir("b")?),
        "b/ recorded as an immediate child of the rebuilt root"
    );
    assert!(
        !map.contains_key(&dir("b/inner")?),
        "b/inner must NOT be loaded: it is outside the target frontier"
    );

    Ok(())
}

/// Derive both paths over a sequence of changesets in topological order and
/// compare the results. Each path writes into its own `MemWritesKeyedBlobstore`
/// overlay for proper isolation (augmented manifest IDs are Hg node hashes, so
/// both paths write the same blobstore key — without separate overlays,
/// `IfAbsent` semantics would mask divergent envelopes).
async fn assert_direct_derivation_parity(
    ctx: &CoreContext,
    repo: &Repo,
    topo_csids: &[ChangesetId],
) -> Result<()> {
    let old_bs = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    let new_bs = Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));

    let mut old_augs: HashMap<ChangesetId, HgAugmentedManifestId> = HashMap::new();
    let mut new_augs: HashMap<ChangesetId, HgAugmentedManifestId> = HashMap::new();

    for cs_id in topo_csids {
        let bonsai: mononoke_types::BonsaiChangeset =
            cs_id.load(ctx, repo.repo_blobstore()).await?;
        let old_parents: Vec<_> = bonsai
            .parents()
            .map(|p| {
                *old_augs
                    .get(&p)
                    .unwrap_or_else(|| panic!("Parent {p} not in topo_csids or out of order"))
            })
            .collect();
        let new_parents: Vec<_> = bonsai
            .parents()
            .map(|p| {
                *new_augs
                    .get(&p)
                    .unwrap_or_else(|| panic!("Parent {p} not in topo_csids or out of order"))
            })
            .collect();
        let mut parents = bonsai.parents();
        let bonsai_parents = (parents.next(), parents.next());

        let old_id = derive_existing_into_overlay(ctx, repo, &*old_bs, *cs_id, old_parents).await?;
        let new_id =
            derive_into_overlay(ctx, repo, &*new_bs, *cs_id, new_parents, bonsai_parents).await?;

        assert_eq!(old_id, new_id, "Root ID mismatch for {cs_id}");

        let old_env = old_id.load(ctx, &*old_bs).await?;
        let new_env = new_id.load(ctx, &*new_bs).await?;
        assert_eq!(
            old_env.augmented_manifest_id, new_env.augmented_manifest_id,
            "Blake3 digest mismatch for {cs_id}"
        );

        compare_augmented_manifests_acl_recursive(
            ctx,
            &*old_bs,
            &*new_bs,
            old_id,
            new_id,
            mononoke_types::MPath::ROOT,
        )
        .await
        .with_context(|| format!("ACL parity failure at {cs_id}"))?;

        old_augs.insert(*cs_id, old_id);
        new_augs.insert(*cs_id, new_id);
    }

    Ok(())
}

/// Octopus copy-from from the two Mercurial-visible parents should preserve
/// Bonsai parent order when matching copy sources to augmented-manifest parents.
#[mononoke::fbinit_test]
async fn test_direct_derivation_octopus_copy_from_respects_hg_parent_order(
    fb: FacebookInit,
) -> Result<()> {
    // Given: three independent parents ordered reverse by ChangesetId, so a
    // bug that sorted parents by id would swap p1/p2 copy-from source slots.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let root_a = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src_a", "content_a")
        .commit()
        .await?;
    let root_b = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src_b", "content_b")
        .commit()
        .await?;
    let root_c = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src_c", "content_c")
        .commit()
        .await?;
    let mut roots = [
        (root_a, "src_a", "content_a"),
        (root_b, "src_b", "content_b"),
        (root_c, "src_c", "content_c"),
    ];
    roots.sort_by_key(|(cs, _, _)| *cs);
    roots.reverse();
    let [(p1, s1, c1), (p2, s2, c2), (p3, _, _)] = roots;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .add_file_with_copy_info("dst_from_p1", c1, (p1, s1))
        .add_file_with_copy_info("dst_from_p2", c2, (p2, s2))
        .commit()
        .await?;

    // When: deriving the octopus merge through both augmented-manifest paths.
    let result = assert_direct_derivation_parity(&ctx, &repo, &[p1, p2, p3, merge]).await;

    // Then: direct derivation matches the existing path's p1/p2 copy-from parentage.
    result
}

/// Octopus copy-from from a step-parent should be ignored, matching Mercurial's
/// two-parent filenode model and the existing HgManifest-based path.
#[mononoke::fbinit_test]
async fn test_direct_derivation_octopus_copy_from_ignores_step_parent(
    fb: FacebookInit,
) -> Result<()> {
    // Given: a three-parent merge whose only copied file names p3 as its source.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src_p1", "content_p1")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src_p2", "content_p2")
        .commit()
        .await?;
    let p3 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src_p3", "content_p3")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
        .add_file_with_copy_info("dst_from_p3", "content_p3", (p3, "src_p3"))
        .commit()
        .await?;

    // When: deriving the octopus merge through both augmented-manifest paths.
    let result = assert_direct_derivation_parity(&ctx, &repo, &[p1, p2, p3, merge]).await;

    // Then: direct derivation matches the existing path, which drops p3 copy-from metadata.
    result
}

#[mononoke::fbinit_test]
async fn test_augmented_manifest_batch_handles_repeated_merge_acl_root(
    fb: FacebookInit,
) -> Result<()> {
    // Given: one derivation batch with two merge commits that share the
    // same non-empty ACL overlay root.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/base.rs", "fn base() {}")
        .commit()
        .await?;
    let left1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("left1.rs", "fn left1() {}")
        .commit()
        .await?;
    let right1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("right1.rs", "fn right1() {}")
        .commit()
        .await?;
    let merge1 = CreateCommitContext::new(&ctx, &repo, vec![left1, right1])
        .commit()
        .await?;
    let left2 = CreateCommitContext::new(&ctx, &repo, vec![merge1])
        .add_file("left2.rs", "fn left2() {}")
        .commit()
        .await?;
    let right2 = CreateCommitContext::new(&ctx, &repo, vec![merge1])
        .add_file("right2.rs", "fn right2() {}")
        .commit()
        .await?;
    let merge2 = CreateCommitContext::new(&ctx, &repo, vec![left2, right2])
        .commit()
        .await?;
    let csids = vec![base, left1, right1, merge1, left2, right2, merge2];
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, csids.clone(), None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
        .await?;
    let acl_roots = manager
        .fetch_derived_batch::<RootAclManifestId>(&ctx, vec![merge1, merge2], None)
        .await?;
    let merge1_acl = derive_hg_augmented_manifest::normalize_acl_root(
        acl_roots.get(&merge1).expect("derived above"),
    )?;
    let merge2_acl = derive_hg_augmented_manifest::normalize_acl_root(
        acl_roots.get(&merge2).expect("derived above"),
    )?;
    assert!(merge1_acl.is_some(), "fixture must have an ACL overlay");
    assert_eq!(merge1_acl, merge2_acl, "fixture must share one ACL root");

    // When: deriving the batch through the manager.
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, csids, None)
        .await?;

    // Then: both merge commits derived through the cached full-ACL-map path
    // still match the Hg manifests.
    for merge in [merge1, merge2] {
        let aug_id = manager
            .fetch_derived::<RootHgAugmentedManifestId>(&ctx, merge, None)
            .await?
            .expect("derived above")
            .hg_augmented_manifest_id();
        let hg_manifest_id = repo
            .derive_hg_changeset(&ctx, merge)
            .await?
            .load(&ctx, repo.repo_blobstore())
            .await?
            .manifestid();
        compare_manifests(&ctx, &repo, hg_manifest_id, aug_id).await?;
    }

    Ok(())
}

/// Verify the full ACL overlay-map cache helper: repeated calls to
/// `cached_acl_overlay_map` with the same `AclManifestId` must reuse the cached
/// `Arc<HashMap<...>>`, not re-walk the tree. This helper is for callers that
/// need a complete ACL path map (for example, later validation tooling); the
/// direct derivation path itself still uses path-scoped overlay loading for
/// non-merge commits.
#[mononoke::fbinit_test]
async fn test_cached_acl_overlay_map_dedupes_within_batch(fb: FacebookInit) -> Result<()> {
    // Given: a non-trivial ACL tree and an initially empty full-overlay cache.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file(
            "restricted/inner/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project2\"\n",
        )
        .add_file("restricted/inner/file.rs", "fn x() {}")
        .add_file("public/readme.md", "hi")
        .commit()
        .await?;
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![cs], None)
        .await?;
    let acl_root = manager
        .fetch_derived::<RootAclManifestId>(&ctx, cs, None)
        .await?
        .expect("derived above");
    let acl_root_overlay = derive_hg_augmented_manifest::normalize_acl_root(&acl_root)?;
    assert!(
        acl_root_overlay.is_some(),
        "Test fixture must produce a non-trivial ACL overlay so the cache \
         hit path is actually exercised.",
    );
    let mut cache = HashMap::new();

    // When: asking for the same full ACL overlay map twice.
    let m1 = derive_hg_augmented_manifest::cached_acl_overlay_map(
        &ctx,
        repo.repo_blobstore(),
        acl_root_overlay,
        &mut cache,
    )
    .await?;
    let m2 = derive_hg_augmented_manifest::cached_acl_overlay_map(
        &ctx,
        repo.repo_blobstore(),
        acl_root_overlay,
        &mut cache,
    )
    .await?;

    // Then: the second call returns the cached Arc and does not add another entry.
    assert!(
        Arc::ptr_eq(&m1, &m2),
        "Repeated calls with the same AclManifestId must return the cached \
         Arc, not a fresh pre-walked map. Cache had {} entries; if the cache \
         were broken the second call would still complete but produce a \
         distinct Arc with the same contents.",
        cache.len(),
    );
    assert_eq!(
        cache.len(),
        1,
        "Cache must contain exactly one entry after two calls with the same root.",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_cached_acl_overlay_map_none_does_not_mutate_cache(fb: FacebookInit) -> Result<()> {
    // Given: an existing full-overlay cache entry and a missing ACL root.
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/file.rs", "fn x() {}")
        .commit()
        .await?;
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![cs], None)
        .await?;
    let acl_root = manager
        .fetch_derived::<RootAclManifestId>(&ctx, cs, None)
        .await?
        .expect("derived above");
    let acl_root_overlay = derive_hg_augmented_manifest::normalize_acl_root(&acl_root)?;
    let mut cache = HashMap::new();
    derive_hg_augmented_manifest::cached_acl_overlay_map(
        &ctx,
        repo.repo_blobstore(),
        acl_root_overlay,
        &mut cache,
    )
    .await?;
    assert_eq!(
        cache.len(),
        1,
        "fixture should seed exactly one cache entry"
    );

    // When: asking for a missing ACL root.
    let m_empty = derive_hg_augmented_manifest::cached_acl_overlay_map(
        &ctx,
        repo.repo_blobstore(),
        None,
        &mut cache,
    )
    .await?;

    // Then: the helper returns an empty map without mutating the cache.
    assert!(m_empty.is_empty());
    assert_eq!(cache.len(), 1, "None must not add a cache entry.");

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_augmented_manifest_skip_writes_uses_mapped_hg_roots(fb: FacebookInit) -> Result<()> {
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);

    // Given: a 3-changeset linear chain with the static manager dependencies
    // retained by this stack already derived.
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a.txt", "initial")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("a.txt", "modified")
        .commit()
        .await?;
    let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
        .add_file("b.txt", "new file")
        .commit()
        .await?;

    let csids = vec![root, child, grandchild];
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<MappedHgChangesetId>(&ctx, csids.clone(), None)
        .await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
        .await?;

    let mut expected_roots = HashMap::new();
    for cs_id in &csids {
        let hg_cs_id = repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, *cs_id)
            .await?
            .with_context(|| format!("Missing MappedHgChangesetId for {cs_id}"))?;
        let expected_root = hg_cs_id
            .load(&ctx, repo.repo_blobstore())
            .await?
            .manifestid()
            .into_nodehash();
        expected_roots.insert(*cs_id, expected_root);
    }

    // When: deriving v1 augmented manifests with Hg manifest skip-writes enabled.
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:hgmanifest_skip_writes".to_string(),
            KnobVal::Bool(true),
        )])),
        async {
            manager
                .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, csids.clone(), None)
                .await
        }
        .boxed(),
    )
    .await?;

    // Then: the derived augmented manifests use the existing mapped Hg roots
    // and keep the manager dependency mapping intact.
    for cs_id in &csids {
        let aug = manager
            .fetch_derived::<RootHgAugmentedManifestId>(&ctx, *cs_id, None)
            .await?
            .with_context(|| format!("Missing RootHgAugmentedManifestId for {cs_id}"))?;
        let aug_envelope: mercurial_types::HgAugmentedManifestEnvelope =
            Loadable::load(&aug.hg_augmented_manifest_id(), &ctx, repo.repo_blobstore()).await?;

        assert_eq!(
            aug_envelope.augmented_manifest.hg_node_id, expected_roots[cs_id],
            "augmented manifest derivation should use the mapped Hg root for {cs_id}",
        );
        assert!(
            repo.bonsai_hg_mapping()
                .get_hg_from_bonsai(&ctx, *cs_id)
                .await?
                .is_some(),
            "manager-level augmented manifest derivation should preserve the mapped Hg dependency for {cs_id}",
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Upload-time ACL node construction.
//
// The oracle throughout is `derive_acl_overlay`: the ACL node the per-changeset
// derivation stamps on the root augmented manifest. Each test rebuilds that
// same node through `acl_manifest::acl_node_for_directory`, which sees no
// changeset and no path, and asserts the two agree.
//
// Every fixture keeps its ACL file at the repository root, so the root
// directory has no child carrying an ACL node and the child map is honestly
// empty. These tests therefore exercise only the directory's own ACL file.
// Children are covered by the `acl_manifest` unit tests, and composition
// across directories end to end by the byte-identity tests of the batch
// builder.
// ---------------------------------------------------------------------------

const TEST_ACL_CONTENT: &str = "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n";

/// The filenode of the ACL file at the root of `cs_id`'s manifest, if any.
async fn root_acl_file(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    acl_file_name: &str,
) -> Result<Option<HgFileNodeId>> {
    let hg_manifest_id = repo
        .derive_hg_changeset(ctx, cs_id)
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid();
    let entry = hg_manifest_id
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            MPath::new(acl_file_name)?,
        )
        .await?;
    Ok(match entry {
        Some(Entry::Leaf((_, filenode_id))) => Some(filenode_id),
        Some(Entry::Tree(_)) | None => None,
    })
}

/// Build the root directory's ACL node the way the tree-upload path does,
/// alongside the node the per-changeset derivation produces for comparison.
async fn upload_and_derived_root_acl(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<(Option<AclManifestId>, Option<AclManifestId>)> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(repo.repo_blobstore().clone());
    let acl_file_name = repo
        .restricted_paths()
        .config_based()
        .config()
        .acl_file_name()
        .to_string();

    // No parent appears anywhere below: the node is a function of this
    // commit's own root directory, which is the property under test.
    let own_acl_file = match root_acl_file(ctx, repo, cs_id, &acl_file_name).await? {
        Some(filenode_id) => Some(filenode_id.load(ctx, &blobstore).await?.content_id()),
        None => None,
    };

    let via_upload = acl_node_for_directory(
        ctx,
        &blobstore,
        DirectoryAclInputs {
            acl_file_name: &acl_file_name,
            own_acl_file,
            children: BTreeMap::new(),
        },
    )
    .await?
    .map(|entry| entry.id);

    let via_derivation = derive_acl_overlay(ctx, repo, cs_id).await?;
    Ok((via_upload, via_derivation))
}

/// A repository with no ACL file has no ACL node anywhere, and the upload path
/// must agree rather than inventing an empty one.
#[mononoke::fbinit_test]
async fn test_upload_acl_node_is_absent_without_an_acl_file(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file.rs", "fn main() {}")
        .commit()
        .await?;

    let (via_upload, via_derivation) = upload_and_derived_root_acl(&ctx, &repo, root).await?;

    assert_eq!(
        via_derivation, None,
        "no .slacl anywhere, so derivation should produce no ACL node"
    );
    assert_eq!(via_upload, via_derivation);
    Ok(())
}

/// Adding the ACL file creates the node, and the upload path must produce the
/// same id from the manifest alone.
#[mononoke::fbinit_test]
async fn test_upload_acl_node_matches_derivation_for_a_new_acl_file(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(".slacl", TEST_ACL_CONTENT)
        .add_file("dir/file.rs", "fn main() {}")
        .commit()
        .await?;

    let (via_upload, via_derivation) = upload_and_derived_root_acl(&ctx, &repo, root).await?;

    assert!(
        via_derivation.is_some(),
        "a root .slacl should produce an ACL node, otherwise this compares nothing"
    );
    assert_eq!(via_upload, via_derivation);
    Ok(())
}

/// A commit that leaves the ACL file alone carries the parent's node forward.
/// This is the case that would silently diverge if the upload path rebuilt the
/// node instead of reusing it.
#[mononoke::fbinit_test]
async fn test_upload_acl_node_carries_forward_an_unchanged_acl_file(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(".slacl", TEST_ACL_CONTENT)
        .add_file("dir/file.rs", "fn main() {}")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir/other.rs", "fn other() {}")
        .commit()
        .await?;

    let parent_acl = derive_acl_overlay(&ctx, &repo, root).await?;
    assert!(parent_acl.is_some(), "the parent should have an ACL node");

    let (via_upload, via_derivation) = upload_and_derived_root_acl(&ctx, &repo, child).await?;

    assert!(
        via_derivation.is_some(),
        "the restriction still exists in the child, so its node should too"
    );
    assert_eq!(via_upload, via_derivation);
    Ok(())
}

/// Deleting the ACL file drops the node rather than reusing the parent's, which
/// is the negative case: reuse here would keep a restriction alive after it was
/// removed.
#[mononoke::fbinit_test]
async fn test_upload_acl_node_is_dropped_when_the_acl_file_is_deleted(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(".slacl", TEST_ACL_CONTENT)
        .add_file("dir/file.rs", "fn main() {}")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file(".slacl")
        .commit()
        .await?;

    // The parent really did have a node, so the child dropping it is a
    // transition and not just two absences.
    let parent_acl = derive_acl_overlay(&ctx, &repo, root).await?;
    assert!(parent_acl.is_some(), "the parent should have an ACL node");

    let (via_upload, via_derivation) = upload_and_derived_root_acl(&ctx, &repo, child).await?;

    assert_eq!(
        via_derivation, None,
        "the only .slacl was deleted, so the node should be gone"
    );
    assert_eq!(via_upload, via_derivation);
    Ok(())
}

async fn hg_manifest_id_of(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<HgManifestId> {
    Ok(repo
        .derive_hg_changeset(ctx, cs_id)
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid())
}

/// The hg manifest id of a directory inside `manifest_id`. An empty path is the
/// root itself.
async fn tree_id_at_path(
    ctx: &CoreContext,
    repo: &Repo,
    manifest_id: HgManifestId,
    path: &str,
) -> Result<HgManifestId> {
    if path.is_empty() {
        return Ok(manifest_id);
    }
    let path = MPath::new(path)?;
    manifest_id
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), path.clone())
        .await?
        .and_then(|entry| entry.into_tree())
        .with_context(|| format!("no directory at {path}"))
}

/// Run one tree through the tree-upload path, reading the uploaded bytes back
/// out of the stored manifest blob the way a client would have sent them.
///
/// The blobstore handed to the builder denies every `hgmanifest.sha1.*` read,
/// so this is also the assertion that the path derives a tree without touching
/// a single legacy manifest blob. The envelope is fetched through the plain
/// blobstore first, because that fetch stands in for the client upload.
async fn build_one_uploaded_tree(
    ctx: &CoreContext,
    repo: &Repo,
    overlay: &Arc<dyn KeyedBlobstore>,
    tree_id: HgManifestId,
) -> Result<()> {
    let envelope = fetch_manifest_envelope(ctx, repo.repo_blobstore(), tree_id).await?;
    let denying: Arc<dyn KeyedBlobstore> = Arc::new(DenyGetKeyedBlobstore::new_denying_prefix(
        overlay.clone(),
        "hgmanifest.sha1.",
    ));
    // A batch of one, so every child directory is resolved from the blobstore.
    let built = build_augmented_manifests_for_uploaded_trees(
        ctx,
        &denying,
        repo.restricted_paths().config_based(),
        vec![envelope],
    )
    .await
    .with_context(|| format!("building uploaded tree {tree_id}"))?;
    // As the store knob does: later builds resolve this tree as an
    // out-of-batch child.
    store_uploaded_tree_envelopes(ctx, overlay, &built).await?;
    Ok(())
}

/// Build each of `build_order` through the tree-upload path, one tree at a
/// time, and assert every envelope is byte-identical to the one the
/// per-changeset derivation produces for the same tree.
///
/// `build_order` is spelled out by the caller rather than computed. A
/// one-tree batch needs every directory inside the tree already derived, and
/// making that dependency explicit is the point: the paths are listed
/// children-first, and `""` is the root.
async fn assert_upload_path_matches_derivation(
    ctx: &CoreContext,
    repo: &Repo,
    parent: ChangesetId,
    child: ChangesetId,
    build_order: &[&str],
) -> Result<()> {
    let parent_manifest = hg_manifest_id_of(ctx, repo, parent).await?;
    let child_manifest = hg_manifest_id_of(ctx, repo, child).await?;
    let restricted_paths_config = repo.restricted_paths().config_based();

    // The upload path reuses the parent's envelopes, so derive them first.
    let parent_root = derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        repo.repo_blobstore(),
        parent_manifest,
        vec![],
        &Default::default(),
        restricted_paths_config,
        derive_acl_overlay(ctx, repo, parent).await?,
    )
    .await?;

    let mut uploaded = Vec::with_capacity(build_order.len());
    for path in build_order {
        uploaded.push(tree_id_at_path(ctx, repo, child_manifest, path).await?);
    }

    // Build via the upload path into an overlay, so the canonical derivation
    // below cannot mask a divergence by overwriting the same keys. Producer F
    // (`derive_hg_augmented_manifest_with_hg_changeset`, on in tests) already
    // stored these envelopes underneath, so hide them: the overlay must only
    // ever hold what the upload path wrote.
    let hidden_inner = DenyGetKeyedBlobstore::missing(
        repo.repo_blobstore().clone(),
        uploaded
            .iter()
            .map(|id| HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key()),
    );
    let overlay: Arc<dyn KeyedBlobstore> = Arc::new(MemWritesKeyedBlobstore::new(hidden_inner));

    for tree_id in &uploaded {
        // A failure here means `build_order` did not list this tree's children
        // before it; the error names the input that was missing.
        build_one_uploaded_tree(ctx, repo, &overlay, *tree_id).await?;
    }

    let mut via_upload = HashMap::new();
    for id in &uploaded {
        let key = HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key();
        let bytes = overlay
            .get(ctx, &key)
            .await?
            .with_context(|| format!("upload path stored no envelope for {id}"))?
            .into_raw_bytes();
        via_upload.insert(*id, bytes);
    }

    let child_root = derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        repo.repo_blobstore(),
        child_manifest,
        vec![parent_root],
        &Default::default(),
        restricted_paths_config,
        derive_acl_overlay(ctx, repo, child).await?,
    )
    .await?;
    assert_eq!(child_root.into_nodehash(), child_manifest.into_nodehash());

    for id in &uploaded {
        let key = HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key();
        let via_derivation = repo
            .repo_blobstore()
            .get(ctx, &key)
            .await?
            .with_context(|| format!("derivation stored no envelope for {id}"))?
            .into_raw_bytes();
        assert_eq!(
            via_upload[id], via_derivation,
            "the upload path envelope for {id} must be byte-identical to the derived one",
        );
    }

    Ok(())
}

/// What it tests: the envelopes the tree-upload path builds are byte-identical
/// to the ones the per-changeset derivation builds for the same trees.
///
/// Why it matters: the blobstore key is the hg node id alone and the digest
/// excludes the ACL pointer, so a divergent envelope would silently win over
/// the canonical one under the default if-absent put behaviour.
#[mononoke::fbinit_test]
async fn test_upload_path_augmented_manifests_are_byte_identical(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("README.md", "hello")
        .add_file("src/lib.rs", "one")
        .add_file("src/deep/nested.rs", "deep")
        .add_file("untouched/file.txt", "untouched")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("src/lib.rs", "two")
        .add_file("src/deep/nested.rs", "deeper")
        .add_file("src/deep/added.rs", "added")
        .add_file("top.txt", "top")
        .commit()
        .await?;

    // The root, src and src/deep are new; untouched/ is reused unchanged, so it
    // is resolved from its stored envelope and spliced in.
    assert_upload_path_matches_derivation(&ctx, &repo, parent, child, &["src/deep", "src", ""])
        .await
}

/// One file past the point where the entry map stops inlining, read off the
/// limit so that raising it cannot silently shrink this fixture below it.
const WIDE_DIRECTORY_FILES: usize = HgAugmentedManifestEntry::WEIGHT_LIMIT + 1;

/// What it tests: a directory too wide for its entry map to inline still builds
/// byte-identically through the tree-upload path.
///
/// Why it matters: canonical derivation splices unchanged runs out of the
/// parent's map while the upload path rebuilds every entry from scratch, and
/// the two are only known to agree while the map inlines into a single node.
/// Every other byte-identity fixture here is small enough that it does, so
/// nothing covers the case where how the map was assembled can change the
/// bytes -- and `PutBehaviour::IfAbsent` makes the first envelope written
/// permanent.
#[mononoke::fbinit_test]
async fn test_upload_path_matches_derivation_above_the_shard_weight_limit(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let wide_files = (0..WIDE_DIRECTORY_FILES)
        .map(|i| Ok((NonRootMPath::new(format!("wide/f{i:05}"))?, format!("v{i}"))))
        .collect::<Result<Vec<_>>>()?;
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_files(wide_files)
        .commit()
        .await?;
    // One changed file out of thousands is what gives canonical derivation a
    // long unchanged run to splice and the upload path nothing to reuse.
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("wide/f00000", "changed")
        .commit()
        .await?;

    assert_upload_path_matches_derivation(&ctx, &repo, parent, child, &["wide", ""]).await?;

    // A fully inlined map weighs exactly its entry count, and a sharded one
    // counts each stored child as one, so a lower weight is proof it sharded.
    let wide = tree_id_at_path(
        &ctx,
        &repo,
        hg_manifest_id_of(&ctx, &repo, child).await?,
        "wide",
    )
    .await?;
    let envelope = HgAugmentedManifestEnvelope::load(
        &ctx,
        repo.repo_blobstore(),
        HgAugmentedManifestId::new(wide.into_nodehash()),
    )
    .await?
    .context("no augmented envelope for the wide directory")?;
    let subentries = &envelope.augmented_manifest.subentries;
    assert!(
        subentries.weight() < subentries.size(),
        "the fixture must exceed the shard weight limit, but its map inlined at weight {}",
        subentries.weight(),
    );

    Ok(())
}

/// What it tests: a directory whose own files are all unchanged is built
/// without reading a single file blob.
///
/// Why it matters: an uploaded manifest lists every file in the directory, not
/// just the changed ones, so rebuilding each leaf cost one filenode load plus
/// one content-metadata lookup per file however little changed. Denying every
/// file read is what keeps that from coming back.
#[mononoke::fbinit_test]
async fn test_upload_path_reuses_unchanged_file_leaves(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let mut files = vec![(NonRootMPath::new("wide/sub/x")?, "one".to_string())];
    for i in 0..8 {
        files.push((NonRootMPath::new(format!("wide/f{i:03}"))?, format!("v{i}")));
    }
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_files(files)
        .commit()
        .await?;
    // Only the subdirectory changes, so every file wide/ lists is unchanged and
    // can only come from the parent's envelope.
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("wide/sub/x", "two")
        .commit()
        .await?;

    let restricted_paths_config = repo.restricted_paths().config_based();
    derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        &ctx,
        repo.repo_blobstore(),
        hg_manifest_id_of(&ctx, &repo, parent).await?,
        vec![],
        &Default::default(),
        restricted_paths_config,
        derive_acl_overlay(&ctx, &repo, parent).await?,
    )
    .await?;

    let child_manifest = hg_manifest_id_of(&ctx, &repo, child).await?;
    let overlay: Arc<dyn KeyedBlobstore> =
        Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    build_one_uploaded_tree(
        &ctx,
        &repo,
        &overlay,
        tree_id_at_path(&ctx, &repo, child_manifest, "wide/sub").await?,
    )
    .await?;

    let no_file_reads: Arc<dyn KeyedBlobstore> = Arc::new(
        DenyGetKeyedBlobstore::new_denying_prefix(overlay.clone(), "hgfilenode.sha1."),
    );
    let wide = tree_id_at_path(&ctx, &repo, child_manifest, "wide").await?;
    build_one_uploaded_tree(&ctx, &repo, &no_file_reads, wide)
        .await
        .context("wide/ must build from the parent's leaves without reading any file blob")?;

    Ok(())
}

/// What it tests: a directory's unchanged child directories are taken from its
/// parent's entries, without loading their envelopes, and the result is still
/// byte-identical to derivation.
///
/// Why it matters: a commit to one user's directory in `notes` rebuilds
/// `users/`, which lists thousands of unchanged children. Loading each of their
/// envelopes put thousands of blobstore reads on the upload path of every such
/// commit. Denying those reads is what keeps that from coming back.
#[mononoke::fbinit_test]
async fn test_upload_path_reuses_unchanged_child_directories(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    const UNCHANGED_CHILDREN: usize = 8;
    let unchanged_paths: Vec<String> = (0..UNCHANGED_CHILDREN)
        .map(|i| format!("wide/d{i:03}"))
        .collect();
    let mut files = vec![(NonRootMPath::new("wide/changed/x")?, "one".to_string())];
    for path in &unchanged_paths {
        files.push((NonRootMPath::new(format!("{path}/y"))?, path.clone()));
    }
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_files(files)
        .commit()
        .await?;
    // Only one child directory changes, so every other child wide/ lists is
    // unchanged and can come from the parent's envelope.
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("wide/changed/x", "two")
        .commit()
        .await?;

    let restricted_paths_config = repo.restricted_paths().config_based();
    derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        &ctx,
        repo.repo_blobstore(),
        hg_manifest_id_of(&ctx, &repo, parent).await?,
        vec![],
        &Default::default(),
        restricted_paths_config,
        derive_acl_overlay(&ctx, &repo, parent).await?,
    )
    .await?;

    let child_manifest = hg_manifest_id_of(&ctx, &repo, child).await?;
    let overlay: Arc<dyn KeyedBlobstore> =
        Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));
    build_one_uploaded_tree(
        &ctx,
        &repo,
        &overlay,
        tree_id_at_path(&ctx, &repo, child_manifest, "wide/changed").await?,
    )
    .await?;

    // Those envelopes exist, so only reuse lets wide/ build without them.
    let mut unchanged_keys = Vec::with_capacity(UNCHANGED_CHILDREN);
    for path in &unchanged_paths {
        let tree = tree_id_at_path(&ctx, &repo, child_manifest, path).await?;
        unchanged_keys.push(HgAugmentedManifestId::new(tree.into_nodehash()).blobstore_key());
    }
    let no_unchanged_child_reads: Arc<dyn KeyedBlobstore> = Arc::new(
        DenyGetKeyedBlobstore::denying_keys(overlay.clone(), unchanged_keys),
    );
    let wide = tree_id_at_path(&ctx, &repo, child_manifest, "wide").await?;
    build_one_uploaded_tree(&ctx, &repo, &no_unchanged_child_reads, wide)
        .await
        .context("wide/ must build from the parent's entries without loading unchanged children")?;

    assert_upload_path_matches_derivation(&ctx, &repo, parent, child, &["wide/changed", "wide", ""])
        .await
}

/// What it tests: a file whose content metadata is missing fails the build
/// instead of having its metadata recomputed.
///
/// Why it matters: content is uploaded before the trees that list it, so a miss
/// means something is already wrong. Recomputing would stream the whole file
/// back inside the user's upload request, an unbounded cost on a best-effort
/// pass.
#[mononoke::fbinit_test]
async fn test_upload_path_fails_on_missing_content_metadata(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    // A root commit, so there is no parent leaf to reuse and the file's leaf
    // has to be built.
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src/deep/nested.rs", "deep")
        .commit()
        .await?;
    let content_id = file_changes_from_bonsai(&ctx, &repo, commit)
        .await?
        .into_iter()
        .find_map(|(_, change)| change.map(|change| change.content_id()))
        .context("the commit adds one file")?;
    let metadata_key = ContentMetadataV2Id::from(content_id).blobstore_key();

    let manifest = hg_manifest_id_of(&ctx, &repo, commit).await?;
    let tree = tree_id_at_path(&ctx, &repo, manifest, "src/deep").await?;
    let without_metadata: Arc<dyn KeyedBlobstore> = Arc::new(DenyGetKeyedBlobstore::missing(
        MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()),
        [metadata_key],
    ));

    let err = build_one_uploaded_tree(&ctx, &repo, &without_metadata, tree)
        .await
        .err()
        .context("a file with no content metadata must fail the build")?;
    assert!(
        format!("{err:#}").contains(&format!("missing content metadata for {content_id}")),
        "the error must name the content, got: {err:#}"
    );

    Ok(())
}

/// What it tests: a directory whose children are all files is built from the
/// uploaded bytes alone, with no dependency on anything else being uploaded.
///
/// Why it matters: this is the atom the rest of the path is made of. `src/deep`
/// has no child directories, so the only inputs are the uploaded manifest, its
/// parent's augmented envelope, and one filenode plus content metadata per
/// file -- and `build_one_uploaded_tree` denies every `hgmanifest.sha1.*` read.
#[mononoke::fbinit_test]
async fn test_upload_path_builds_a_leaf_directory_alone(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src/deep/nested.rs", "deep")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("src/deep/nested.rs", "deeper")
        .add_file("src/deep/added.rs", "added")
        .commit()
        .await?;

    assert_upload_path_matches_derivation(&ctx, &repo, parent, child, &["src/deep"]).await
}

/// What it tests: a tree whose child directory has not been derived yet fails,
/// and leaves nothing behind.
///
/// Why it matters: that child is a precondition, not a guarantee. The entry the
/// tree has to record for it carries the child's augmented id and size, so
/// building anyway would either invent them or silently reuse the parent's
/// stale child -- and `PutBehaviour::IfAbsent` would make that permanent.
/// Reporting success having built nothing would be just as bad: the coverage
/// hole this path exists to close would reopen silently.
#[mononoke::fbinit_test]
async fn test_upload_path_fails_when_a_child_is_not_derived(fb: FacebookInit) -> Result<()> {
    with_just_knobs_async(
        // `derive_hg_changeset` derives augmented manifests alongside the hg
        // changeset. Left on, the setup below would derive the child commit's
        // trees and leave no undrived child to test against.
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string(),
            KnobVal::Bool(false),
        )])),
        async {
            let ctx = CoreContext::test_mock(fb);
            let repo: Repo = test_repo_factory::build_empty(fb).await?;

            let parent = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("src/deep/nested.rs", "deep")
                .commit()
                .await?;
            let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
                .add_file("src/deep/nested.rs", "deeper")
                .commit()
                .await?;

            let parent_manifest = hg_manifest_id_of(&ctx, &repo, parent).await?;
            let child_manifest = hg_manifest_id_of(&ctx, &repo, child).await?;
            derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
                &ctx,
                repo.repo_blobstore(),
                parent_manifest,
                vec![],
                &Default::default(),
                repo.restricted_paths().config_based(),
                derive_acl_overlay(&ctx, &repo, parent).await?,
            )
            .await?;

            let overlay: Arc<dyn KeyedBlobstore> =
                Arc::new(MemWritesKeyedBlobstore::new(repo.repo_blobstore().clone()));

            // `src` is built without building `src/deep` first, so the child's
            // new augmented manifest does not exist.
            let src = tree_id_at_path(&ctx, &repo, child_manifest, "src").await?;
            let deep = tree_id_at_path(&ctx, &repo, child_manifest, "src/deep").await?;
            let err = build_one_uploaded_tree(&ctx, &repo, &overlay, src)
                .await
                .expect_err("a tree whose child is not derived must fail, not build");
            let message = format!("{err:#}");
            assert!(
                message.contains("deep") && message.contains("already derived"),
                "the error should name the missing input and why, got: {message}"
            );
            // The handler's counter split downcasts to this variant, so a
            // matching message alone is not enough.
            let typed = err
                .chain()
                .find_map(|cause| cause.downcast_ref::<UploadTreeBuildError>());
            assert!(
                matches!(
                    typed,
                    Some(UploadTreeBuildError::MissingChild { tree, name, child })
                        if *tree == src.into_nodehash()
                            && name.as_ref() == b"deep"
                            && *child == deep.into_nodehash()
                ),
                "the error should be MissingChild for src/deep, got: {typed:?}"
            );

            let key = HgAugmentedManifestId::new(src.into_nodehash()).blobstore_key();
            assert!(
                overlay.get(&ctx, &key).await?.is_none(),
                "a failed tree must not leave an envelope behind"
            );

            // Building the child first is all it takes for the same call to
            // succeed.
            build_one_uploaded_tree(&ctx, &repo, &overlay, deep).await?;
            build_one_uploaded_tree(&ctx, &repo, &overlay, src).await?;

            Ok(())
        }
        .boxed(),
    )
    .await
}

/// Same byte-identity gate with ACL pointers on: the pointer is excluded from
/// the digest but not from the stored envelope, so it has to match too.
#[mononoke::fbinit_test]
async fn test_upload_path_augmented_manifests_are_byte_identical_with_slacl(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/code/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/code/secret.rs", "fn secret() {}")
        .add_file("restricted/other/plain.rs", "fn plain() {}")
        .add_file("public/readme.md", "hello")
        .commit()
        .await?;
    // Adds a second restriction root, so the child changes the ACL tree too.
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("restricted/code/more.rs", "fn more() {}")
        .add_file(
            "restricted/other/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project2\"\n",
        )
        .commit()
        .await?;
    let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
        .delete_file("restricted/other/.slacl")
        .commit()
        .await?;

    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:add_acl_manifest_pointer".to_string(),
            KnobVal::Bool(true),
        )])),
        async {
            // The root, restricted, restricted/code and restricted/other are new.
            assert_upload_path_matches_derivation(
                &ctx,
                &repo,
                parent,
                child,
                &["restricted/code", "restricted/other", "restricted", ""],
            )
            .await?;
            // Removing the ACL file has to drop the ACL node, not reuse it.
            assert_upload_path_matches_derivation(
                &ctx,
                &repo,
                child,
                grandchild,
                &["restricted/other", "restricted", ""],
            )
            .await
        }
        .boxed(),
    )
    .await
}

/// Rebuild the client-side view of a set of uploaded trees from the stored
/// Mercurial manifest blobs.
async fn uploaded_trees_for(
    ctx: &CoreContext,
    repo: &Repo,
    ids: &[HgManifestId],
) -> Result<Vec<HgManifestEnvelope>> {
    let mut trees = Vec::with_capacity(ids.len());
    for id in ids {
        trees.push(fetch_manifest_envelope(ctx, repo.repo_blobstore(), *id).await?);
    }
    Ok(trees)
}

/// What it tests: handed every directory at once in hash order, the batch
/// entry point orders them itself and produces the same bytes as the
/// per-changeset derivation.
///
/// Why it matters: a one-tree batch requires its children to exist already,
/// so ordering is the batch layer's whole job. Feeding it hash order
/// means the fixture cannot accidentally be in dependency order.
async fn assert_batch_upload_matches_derivation(
    ctx: &CoreContext,
    repo: &Repo,
    parents: &[ChangesetId],
    child: ChangesetId,
    paths: &[&str],
) -> Result<()> {
    let child_manifest = hg_manifest_id_of(ctx, repo, child).await?;
    let restricted_paths_config = repo.restricted_paths().config_based();

    let mut parent_manifests = Vec::with_capacity(parents.len());
    let mut parent_roots = Vec::with_capacity(parents.len());
    for parent in parents {
        let parent_manifest = hg_manifest_id_of(ctx, repo, *parent).await?;
        parent_roots.push(
            derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
                ctx,
                repo.repo_blobstore(),
                parent_manifest,
                vec![],
                &Default::default(),
                restricted_paths_config,
                derive_acl_overlay(ctx, repo, *parent).await?,
            )
            .await?,
        );
        parent_manifests.push(parent_manifest);
    }

    let mut uploaded = Vec::with_capacity(paths.len());
    for path in paths {
        let id = tree_id_at_path(ctx, repo, child_manifest, path).await?;
        // A tree a parent already has is resolved from its stored envelope, not
        // uploaded, and hiding that envelope below would break the build.
        for parent_manifest in &parent_manifests {
            let in_parent = tree_id_at_path(ctx, repo, *parent_manifest, path)
                .await
                .ok();
            assert_ne!(
                in_parent,
                Some(id),
                "{path:?} is unchanged from a parent, so it would not be uploaded"
            );
        }
        uploaded.push(id);
    }
    uploaded.sort_by_key(|id| id.into_nodehash());

    // Producer F (`derive_hg_augmented_manifest_with_hg_changeset`, on in
    // tests) already stored these envelopes underneath, so hide them: the
    // overlay must only ever hold what the upload path wrote.
    let hidden_inner = DenyGetKeyedBlobstore::missing(
        repo.repo_blobstore().clone(),
        uploaded
            .iter()
            .map(|id| HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key()),
    );
    let overlay: Arc<dyn KeyedBlobstore> = Arc::new(MemWritesKeyedBlobstore::new(hidden_inner));
    let outcomes = build_augmented_manifests_for_uploaded_trees(
        ctx,
        &overlay,
        restricted_paths_config,
        uploaded_trees_for(ctx, repo, &uploaded).await?,
    )
    .await?;
    assert_eq!(
        outcomes.len(),
        uploaded.len(),
        "every tree in the batch should have been built"
    );
    for id in &uploaded {
        let key = HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key();
        assert!(
            overlay.get(ctx, &key).await?.is_none(),
            "the build should leave storing {id}'s envelope to store_uploaded_tree_envelopes",
        );
    }
    store_uploaded_tree_envelopes(ctx, &overlay, &outcomes).await?;

    let mut via_upload = HashMap::new();
    for id in &uploaded {
        let key = HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key();
        let bytes = overlay
            .get(ctx, &key)
            .await?
            .with_context(|| format!("upload path stored no envelope for {id}"))?
            .into_raw_bytes();
        via_upload.insert(*id, bytes);
    }

    let child_root = derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        repo.repo_blobstore(),
        child_manifest,
        parent_roots,
        &Default::default(),
        restricted_paths_config,
        derive_acl_overlay(ctx, repo, child).await?,
    )
    .await?;
    assert_eq!(child_root.into_nodehash(), child_manifest.into_nodehash());

    for id in &uploaded {
        let key = HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key();
        let via_derivation = repo
            .repo_blobstore()
            .get(ctx, &key)
            .await?
            .with_context(|| format!("derivation stored no envelope for {id}"))?
            .into_raw_bytes();
        assert_eq!(
            via_upload[id], via_derivation,
            "the batch envelope for {id} must be byte-identical to the derived one",
        );
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_upload_augmented_manifests_are_byte_identical(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("README.md", "hello")
        .add_file("src/lib.rs", "one")
        .add_file("src/deep/nested.rs", "deep")
        .add_file("untouched/file.txt", "untouched")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("src/lib.rs", "two")
        .add_file("src/deep/nested.rs", "deeper")
        .add_file("src/deep/added.rs", "added")
        .add_file("top.txt", "top")
        .commit()
        .await?;

    assert_batch_upload_matches_derivation(&ctx, &repo, &[parent], child, &["src/deep", "src", ""])
        .await
}

/// What it tests: the batch path matches derivation with ACL pointers on, for
/// a commit that adds an ACL file and one that deletes it.
///
/// Why it matters: a child built in the same batch hands its ACL flags to its
/// parent directly, where a child built alone has them read back from the
/// blobstore. Production uploads whole trees in one batch, so this is the path
/// that decides the ACL pointers mapping later relies on, and the one-tree
/// `.slacl` test never reaches it.
#[mononoke::fbinit_test]
async fn test_batch_upload_augmented_manifests_are_byte_identical_with_slacl(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/code/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/code/secret.rs", "fn secret() {}")
        .add_file("restricted/other/plain.rs", "fn plain() {}")
        .add_file("public/readme.md", "hello")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("restricted/code/more.rs", "fn more() {}")
        .add_file(
            "restricted/other/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project2\"\n",
        )
        .commit()
        .await?;
    let grandchild = CreateCommitContext::new(&ctx, &repo, vec![child])
        .delete_file("restricted/other/.slacl")
        .commit()
        .await?;

    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:add_acl_manifest_pointer".to_string(),
            KnobVal::Bool(true),
        )])),
        async {
            assert_batch_upload_matches_derivation(
                &ctx,
                &repo,
                &[parent],
                child,
                &["restricted/code", "restricted/other", "restricted", ""],
            )
            .await?;
            assert_batch_upload_matches_derivation(
                &ctx,
                &repo,
                &[child],
                grandchild,
                &["restricted/other", "restricted", ""],
            )
            .await
        }
        .boxed(),
    )
    .await
}

/// What it tests: the batch path matches derivation for a merge whose parents
/// both changed a restricted directory, one of them by adding a second ACL file.
///
/// Why it matters: for a merge, derivation may reuse a parent's envelope, and
/// at the root only a content-derived one, while the upload path always builds
/// its own under the id the client sent. Every other byte-identity fixture here
/// has one parent, so nothing shows the two agree on a merge.
#[mononoke::fbinit_test]
async fn test_batch_upload_augmented_manifests_are_byte_identical_for_a_merge(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "restricted/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project1\"\n",
        )
        .add_file("restricted/secret.rs", "fn secret() {}")
        .add_file("public/readme.md", "hello")
        .commit()
        .await?;
    let p1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("restricted/from_p1.rs", "fn p1() {}")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("restricted/from_p2.rs", "fn p2() {}")
        .add_file(
            "restricted/inner/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=project2\"\n",
        )
        .add_file("restricted/inner/deep.rs", "fn deep() {}")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .commit()
        .await?;

    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:add_acl_manifest_pointer".to_string(),
            KnobVal::Bool(true),
        )])),
        async {
            // restricted/inner comes from p2 unchanged, so only the directory
            // both parents touched and the root are new.
            assert_batch_upload_matches_derivation(
                &ctx,
                &repo,
                &[p1, p2],
                merge,
                &["restricted", ""],
            )
            .await
        }
        .boxed(),
    )
    .await
}

/// What it tests: a directory whose child directory is named like the ACL
/// file and holds a restriction, uploaded as a client would, builds the same
/// envelopes and ACL nodes as canonical derivation.
///
/// Why it matters: the tree is valid and canonical derivation builds it. The
/// upload path used to reject it as colliding with the directory's own ACL
/// file. A rejected batch stores nothing, and every later upload listing the
/// same subtree then fails with a missing child, because clients never resend
/// trees the server already has.
#[mononoke::fbinit_test]
async fn test_upload_of_a_child_directory_named_like_the_acl_file(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("d/.slacl/.slacl", TEST_ACL_CONTENT)
        .add_file("d/file", "file")
        .commit()
        .await?;
    let paths = ["d/.slacl", "d", ""];

    // Envelopes, including their ACL pointers, are byte-identical.
    assert_batch_upload_matches_derivation(&ctx, &repo, &[], root, &paths).await?;

    // The ACL nodes the upload path hands up match canonical derivation's.
    let manifest = hg_manifest_id_of(&ctx, &repo, root).await?;
    let mut uploaded = Vec::new();
    for path in paths {
        uploaded.push(tree_id_at_path(&ctx, &repo, manifest, path).await?);
    }
    let overlay: Arc<dyn KeyedBlobstore> = Arc::new(MemWritesKeyedBlobstore::new(
        DenyGetKeyedBlobstore::missing(
            repo.repo_blobstore().clone(),
            uploaded
                .iter()
                .map(|id| HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key()),
        ),
    ));
    let built = build_augmented_manifests_for_uploaded_trees(
        &ctx,
        &overlay,
        repo.restricted_paths().config_based(),
        uploaded_trees_for(&ctx, &repo, &uploaded).await?,
    )
    .await?;
    let acl_root = derive_acl_overlay(&ctx, &repo, root)
        .await?
        .context("d/.slacl/ holds a restriction, so there is an ACL manifest")?;
    for (path, id) in paths.into_iter().zip(&uploaded) {
        let tree = built
            .iter()
            .find(|tree| tree.node_id == id.into_nodehash())
            .with_context(|| format!("{path:?} was not built"))?;
        let mpath = if path.is_empty() {
            MPath::ROOT
        } else {
            MPath::new(path)?
        };
        let canonical = match acl_root
            .find_entry(ctx.clone(), repo.repo_blobstore().clone(), mpath)
            .await?
        {
            Some(Entry::Tree(canonical)) => canonical,
            other => panic!("{path:?} should have a canonical ACL node, got {other:?}"),
        };
        let canonical_node = canonical.load(&ctx, repo.repo_blobstore()).await?;
        let DirectoryAcl::Node(entry) = &tree.acl else {
            panic!(
                "{path:?} should have an uploaded ACL node, got {:?}",
                tree.acl
            );
        };
        assert_eq!(entry.id, canonical, "{path:?}: ACL node ids");
        assert_eq!(
            entry.is_restricted,
            matches!(
                canonical_node.restriction,
                AclManifestDirectoryRestriction::Restricted(_)
            ),
            "{path:?}: is_restricted"
        );
        assert_eq!(
            entry.has_restricted_descendants,
            canonical_node.subentries.rollup_data().has_restricted,
            "{path:?}: has_restricted_descendants"
        );
    }

    Ok(())
}

/// What it tests: a directory whose `.slacl` is unchanged gets the ACL entry
/// its parent recorded, even when parsing the file today would build another.
///
/// Why it matters: canonical derivation never re-parses an unchanged ACL file,
/// so after a parser change, or with upload and derivation on different
/// versions, a re-parse at upload stores a permanent envelope that disagrees
/// with canonical. An older parser is modelled by recording another commit's
/// ACL tree, built from different content, as the parent's.
#[mononoke::fbinit_test]
async fn test_batch_upload_reuses_parent_acl_entry_for_an_unchanged_acl_file(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(repo.repo_blobstore().clone());

    let older = CreateCommitContext::new_root(&ctx, &repo)
        .add_file(
            "dir/.slacl",
            "repo_region_acl = \"REPO_REGION:repos/hg/fbsource/=older\"\n",
        )
        .commit()
        .await?;
    let parent = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/.slacl", TEST_ACL_CONTENT)
        .add_file("dir/file.rs", "one")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("dir/file.rs", "two")
        .commit()
        .await?;

    // Must come before anything derives `parent`, which would write its mapping.
    let manager = repo.repo_derived_data().manager();
    let older_acl = derive_acl_overlay(&ctx, &repo, older).await?;
    manager
        .fetch_derived::<RootAclManifestId>(&ctx, older, None)
        .await?
        .context("older was just derived")?
        .store_mapping(&ctx, &manager.derivation_context(None), parent)
        .await?;

    let canonical_root = derive_acl_overlay(&ctx, &repo, child).await?;
    assert_eq!(
        canonical_root, older_acl,
        "canonical derivation carries the parent's recorded ACL tree forward"
    );
    let canonical_dir = canonical_root
        .context("the child is restricted")?
        .load(&ctx, &blobstore)
        .await?
        .subentries
        .lookup(&ctx, &blobstore, b"dir")
        .await?;

    let child_manifest = hg_manifest_id_of(&ctx, &repo, child).await?;
    let dir = tree_id_at_path(&ctx, &repo, child_manifest, "dir").await?;
    let built = build_augmented_manifests_for_uploaded_trees(
        &ctx,
        &blobstore,
        repo.restricted_paths().config_based(),
        uploaded_trees_for(&ctx, &repo, &[dir, child_manifest]).await?,
    )
    .await?;
    let upload_dir = built
        .into_iter()
        .find(|tree| tree.node_id == dir.into_nodehash())
        .and_then(|tree| match tree.acl {
            DirectoryAcl::Node(entry) => Some(AclManifestEntry::Directory(entry)),
            DirectoryAcl::NotNeeded | DirectoryAcl::Empty => None,
        });

    assert_eq!(
        upload_dir, canonical_dir,
        "the upload reuses the parent's ACL entry, id and flags, without re-parsing"
    );
    assert_batch_upload_matches_derivation(&ctx, &repo, &[parent], child, &["dir", ""]).await
}

/// A batch whose trees cannot all be built fails outright, and stores nothing.
///
/// A child that is neither in the batch nor already derived means the client
/// broke the children-before-parents contract, and nothing about the rest of
/// the batch is trustworthy once that is true.
async fn assert_upload_fails_with_missing_children(
    ctx: &CoreContext,
    repo: &Repo,
    uploaded: &[HgManifestId],
    missing: &[HgManifestId],
    failing_tree: HgManifestId,
) -> Result<()> {
    let hidden_inner = DenyGetKeyedBlobstore::missing(
        repo.repo_blobstore().clone(),
        uploaded
            .iter()
            .chain(missing)
            .map(|id| HgAugmentedManifestId::new(id.into_nodehash()).blobstore_key()),
    );
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(MemWritesKeyedBlobstore::new(hidden_inner));
    let err = build_augmented_manifests_for_uploaded_trees(
        ctx,
        &blobstore,
        repo.restricted_paths().config_based(),
        uploaded_trees_for(ctx, repo, uploaded).await?,
    )
    .await
    .expect_err("a missing child should fail the uploaded tree batch");

    assert!(
        format!("{err:#}").contains(&failing_tree.to_string()),
        "the error should identify the first tree whose child is missing",
    );
    for id in uploaded {
        assert!(
            HgAugmentedManifestEnvelope::load(
                ctx,
                &blobstore,
                HgAugmentedManifestId::new(id.into_nodehash()),
            )
            .await?
            .is_none(),
            "a failed batch should not store an envelope for {id}",
        );
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_upload_fails_when_direct_child_is_missing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("child/file.rs", "fn main() {}")
        .commit()
        .await?;
    let root = hg_manifest_id_of(&ctx, &repo, cs_id).await?;
    let child = tree_id_at_path(&ctx, &repo, root, "child").await?;

    assert_upload_fails_with_missing_children(&ctx, &repo, &[root], &[child], root).await
}

#[mononoke::fbinit_test]
async fn test_upload_fails_when_transitive_child_is_missing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("child/grandchild/file.rs", "fn main() {}")
        .commit()
        .await?;
    let root = hg_manifest_id_of(&ctx, &repo, cs_id).await?;
    let child = tree_id_at_path(&ctx, &repo, root, "child").await?;
    let grandchild = tree_id_at_path(&ctx, &repo, root, "child/grandchild").await?;

    assert_upload_fails_with_missing_children(
        &ctx,
        &repo,
        &[root, child],
        &[child, grandchild],
        child,
    )
    .await
}
