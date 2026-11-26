/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Rebuilding Hg trees from augmented manifests, and storing the missing ones.

use std::collections::BTreeMap;
use std::collections::HashMap;

use anyhow::Context;
use anyhow::Result;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bytes::Bytes;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use manifest::ManifestOps;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::rebuild_hg_manifests::CommitHgRoots;
use mercurial_derivation::rebuild_hg_manifests::HgTreeRebuilder;
use mercurial_derivation::rebuild_hg_manifests::RebuildAction;
use mercurial_derivation::rebuild_hg_manifests::TreeOutcome;
use mercurial_derivation::rebuild_hg_manifests::TreeScope;
use mercurial_derivation::rebuild_hg_manifests::commit_hg_roots;
use mercurial_types::FileType;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgManifestEnvelope;
use mercurial_types::HgManifestEnvelopeMut;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::NULL_HASH;
use mercurial_types::blobs::HgBlobChangeset;
use metaconfig_types::PathRestrictionMetadata;
use metaconfig_types::RestrictedPathsConfig;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::sha1_hash::Sha1;
use mononoke_types::sharded_map_v2::ShardedMapV2Value;
use permission_checker::MononokeIdentity;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths::ManifestType;
use restricted_paths::RestrictedPathsRef;
use tests_utils::CreateCommitContext;

use crate::Repo;

/// Outcome of each rebuilt tree, by path, with "" for the root.
type Outcomes = BTreeMap<String, (HgManifestId, TreeOutcome)>;

async fn roots(ctx: &CoreContext, repo: &Repo, cs_id: ChangesetId) -> Result<CommitHgRoots> {
    commit_hg_roots(ctx, repo.repo_blobstore(), repo.bonsai_hg_mapping(), cs_id).await
}

fn rebuilder(repo: &Repo) -> HgTreeRebuilder<RepoBlobstore> {
    HgTreeRebuilder::new(
        repo.repo_blobstore().clone(),
        repo.restricted_paths().config_based().clone(),
    )
}

async fn rebuild(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    scope: TreeScope,
    action: RebuildAction,
) -> Result<Outcomes> {
    let roots = roots(ctx, repo, cs_id).await?;
    let rebuilt: Vec<_> = rebuilder(repo)
        .rebuild(ctx, roots, scope, action)
        .try_collect()
        .await?;
    let count = rebuilt.len();
    let outcomes: Outcomes = rebuilt
        .into_iter()
        .map(|tree| (tree.path.to_string(), (tree.tree, tree.outcome)))
        .collect();
    assert_eq!(outcomes.len(), count, "each path should be rebuilt once");
    Ok(outcomes)
}

fn paths_with(outcomes: &Outcomes, outcome: TreeOutcome) -> Vec<&str> {
    outcomes
        .iter()
        .filter(|(_, (_, o))| *o == outcome)
        .map(|(path, _)| path.as_str())
        .collect()
}

async fn hg_root(ctx: &CoreContext, repo: &Repo, cs_id: ChangesetId) -> Result<HgManifestId> {
    Ok(repo
        .derive_hg_changeset(ctx, cs_id)
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid())
}

/// The Hg trees of `cs_id`'s whole manifest, by path.
async fn hg_tree_paths(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
) -> Result<BTreeMap<String, HgManifestId>> {
    let root = hg_root(ctx, repo, cs_id).await?;
    root.list_tree_entries(ctx.clone(), repo.repo_blobstore().clone())
        .map_ok(|(path, tree)| (path.to_string(), tree))
        .try_collect()
        .await
}

async fn tree_at(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    path: &str,
) -> Result<HgManifestId> {
    hg_tree_paths(ctx, repo, cs_id)
        .await?
        .remove(path)
        .with_context(|| format!("no tree at {path:?}"))
}

async fn stored_bytes(ctx: &CoreContext, repo: &Repo, tree: HgManifestId) -> Result<Option<Bytes>> {
    Ok(repo
        .repo_blobstore()
        .get(ctx, &tree.blobstore_key())
        .await?
        .map(BlobstoreGetData::into_raw_bytes))
}

/// Remove the stored Hg trees `trees`, returning their bytes.
async fn unlink_trees(
    ctx: &CoreContext,
    repo: &Repo,
    trees: &[HgManifestId],
) -> Result<Vec<Bytes>> {
    let mut saved = Vec::new();
    for tree in trees {
        saved.push(
            stored_bytes(ctx, repo, *tree)
                .await?
                .with_context(|| format!("tree {tree} should be stored"))?,
        );
        repo.repo_blobstore()
            .unlink(ctx, &tree.blobstore_key())
            .await?;
    }
    Ok(saved)
}

/// What it tests: every tree rebuilt from augmented manifests has the bytes
/// of the Hg tree derivation stored, across file types, deletions and a merge,
/// and a commit's new trees are exactly those that differ from every parent.
#[mononoke::fbinit_test]
async fn test_rebuilt_trees_equal_stored_hg_trees(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a", "a")
        .add_file_with_type("dir/run", "#!/bin/sh", FileType::Executable)
        .add_file_with_type("dir/link", "a", FileType::Symlink)
        .add_file("dir/sub/c", "c")
        .add_file("keep/f", "f")
        .add_file("other/d", "d")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir/sub/c", "changed")
        .add_file("new/e", "e")
        .delete_file("other/d")
        .commit()
        .await?;
    let side = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("x/y", "y")
        .commit()
        .await?;
    // A merge must resolve every file its parents disagree on.
    let merge = CreateCommitContext::new(&ctx, &repo, vec![child, side])
        .add_file("dir/sub/c", "changed")
        .delete_file("other/d")
        .commit()
        .await?;

    for cs_id in [root, child, side, merge] {
        let expected = hg_tree_paths(&ctx, &repo, cs_id).await?;
        let outcomes =
            rebuild(&ctx, &repo, cs_id, TreeScope::Whole, RebuildAction::Compare).await?;
        let rebuilt: BTreeMap<String, HgManifestId> = outcomes
            .iter()
            .map(|(path, (tree, _))| (path.clone(), *tree))
            .collect();
        assert_eq!(rebuilt, expected, "every tree of {cs_id} should be rebuilt");
        assert_eq!(
            paths_with(&outcomes, TreeOutcome::Identical).len(),
            expected.len(),
            "every rebuilt tree of {cs_id} should equal the stored one: {outcomes:?}",
        );
    }

    // The root, the changed directories and the added one; `keep` is the
    // parent's, and `other` is gone.
    let new = rebuild(
        &ctx,
        &repo,
        child,
        TreeScope::NewInCommit,
        RebuildAction::Compare,
    )
    .await?;
    assert_eq!(
        paths_with(&new, TreeOutcome::Identical),
        vec!["", "dir", "dir/sub", "new"],
    );
    assert_eq!(
        new.len(),
        4,
        "only the new trees should be rebuilt: {new:?}"
    );

    // A merge's new trees are those whose Hg tree differs from the one at the
    // same path in both parents, by the Hg manifests derivation stored.
    let (merge_trees, child_trees, side_trees) = (
        hg_tree_paths(&ctx, &repo, merge).await?,
        hg_tree_paths(&ctx, &repo, child).await?,
        hg_tree_paths(&ctx, &repo, side).await?,
    );
    let expected: Vec<&str> = merge_trees
        .iter()
        .filter(|(path, tree)| {
            child_trees.get(*path) != Some(*tree) && side_trees.get(*path) != Some(*tree)
        })
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(
        expected.contains(&"") && !expected.contains(&"x") && !expected.contains(&"new"),
        "fixture: the merge's root should be new, and x and new should come from a parent: {expected:?}",
    );
    let new = rebuild(
        &ctx,
        &repo,
        merge,
        TreeScope::NewInCommit,
        RebuildAction::Compare,
    )
    .await?;
    assert_eq!(paths_with(&new, TreeOutcome::Identical), expected);
    assert_eq!(
        new.len(),
        expected.len(),
        "only the merge's new trees should be rebuilt: {new:?}"
    );

    Ok(())
}

/// What it tests: missing Hg trees are reported and left missing unless
/// storing is asked for, and then are stored with the bytes derivation wrote.
#[mononoke::fbinit_test]
async fn test_missing_trees_are_stored_only_when_asked(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/sub/c", "c")
        .add_file("keep/f", "f")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir/sub/c", "changed")
        .add_file("new/e", "e")
        .commit()
        .await?;
    hg_root(&ctx, &repo, child).await?;

    let new_paths = ["", "dir", "dir/sub", "new"];
    let mut trees = Vec::new();
    for path in new_paths {
        trees.push(tree_at(&ctx, &repo, child, path).await?);
    }
    let saved = unlink_trees(&ctx, &repo, &trees).await?;

    let compared = rebuild(
        &ctx,
        &repo,
        child,
        TreeScope::NewInCommit,
        RebuildAction::Compare,
    )
    .await?;
    assert_eq!(paths_with(&compared, TreeOutcome::Missing), new_paths);
    for tree in &trees {
        assert_eq!(
            stored_bytes(&ctx, &repo, *tree).await?,
            None,
            "comparing should not store {tree}",
        );
    }

    let stored = rebuild(
        &ctx,
        &repo,
        child,
        TreeScope::NewInCommit,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(paths_with(&stored, TreeOutcome::Stored), new_paths);
    for (tree, original) in trees.iter().zip(saved) {
        assert_eq!(
            stored_bytes(&ctx, &repo, *tree).await?,
            Some(original),
            "the stored tree {tree} should have the bytes derivation wrote",
        );
    }

    let again = rebuild(&ctx, &repo, child, TreeScope::Whole, RebuildAction::Compare).await?;
    assert_eq!(
        paths_with(&again, TreeOutcome::Identical).len(),
        again.len(),
        "after storing, every tree should match: {again:?}",
    );
    Ok(())
}

/// One file past the point where the augmented entry map stops inlining.
const WIDE_DIRECTORY_FILES: usize = HgAugmentedManifestEntry::WEIGHT_LIMIT + 1;

/// What it tests: a directory whose augmented entries are sharded across
/// several blobs rebuilds to the Hg tree derivation stored.
#[mononoke::fbinit_test]
async fn test_sharded_directory_rebuilds_byte_identically(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let wide_files = (0..WIDE_DIRECTORY_FILES)
        .map(|i| Ok((NonRootMPath::new(format!("wide/f{i:05}"))?, format!("v{i}"))))
        .collect::<Result<Vec<_>>>()?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_files(wide_files)
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("wide/f00000", "changed")
        .commit()
        .await?;
    hg_root(&ctx, &repo, child).await?;

    let wide = tree_at(&ctx, &repo, child, "wide").await?;
    let envelope = HgAugmentedManifestId::new(wide.into_nodehash())
        .load(&ctx, repo.repo_blobstore())
        .await?;
    let subentries = &envelope.augmented_manifest.subentries;
    assert!(
        subentries.weight() < subentries.size(),
        "the fixture must shard its entries, but they inlined at weight {}",
        subentries.weight(),
    );

    let root_tree = tree_at(&ctx, &repo, child, "").await?;
    let saved = unlink_trees(&ctx, &repo, &[root_tree, wide]).await?;
    let stored = rebuild(
        &ctx,
        &repo,
        child,
        TreeScope::NewInCommit,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(paths_with(&stored, TreeOutcome::Stored), vec!["", "wide"]);
    assert_eq!(
        stored_bytes(&ctx, &repo, root_tree).await?,
        Some(saved[0].clone())
    );
    assert_eq!(
        stored_bytes(&ctx, &repo, wide).await?,
        Some(saved[1].clone()),
        "the sharded directory should rebuild to the bytes derivation wrote",
    );
    Ok(())
}

fn supplied_root_hash() -> HgNodeHash {
    HgNodeHash::new(Sha1::from_byte_array([0xAB; 20]))
}

/// What it tests: a root tree stored under a client-chosen id, which is not
/// the hash of its contents, is found through the commit's Hg changeset and
/// rebuilt under that id with the bytes the client's upload stored.
#[mononoke::fbinit_test]
async fn test_client_chosen_root_id_rebuilds_under_that_id(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/a", "a")
        .commit()
        .await?;
    let commit = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("dir/p", "p")
        .add_file("new/q", "q")
        .commit()
        .await?;
    let supplied = supplied_root_hash();
    fixtures::store_supplied_hg_root(&ctx, &repo, commit, supplied).await?;
    repo.repo_derived_data()
        .manager()
        .derive_heads::<RootHgAugmentedManifestId>(ctx.clone(), vec![commit], None, None)
        .await?;

    let roots = roots(&ctx, &repo, commit).await?;
    assert_eq!(roots.root.into_nodehash(), supplied);
    let augmented = HgAugmentedManifestId::new(supplied)
        .load(&ctx, repo.repo_blobstore())
        .await?;
    assert_ne!(
        augmented.augmented_manifest.hg_node_id, augmented.augmented_manifest.computed_node_id,
        "fixture: the root should be stored under an id that is not its content hash",
    );

    let saved = unlink_trees(&ctx, &repo, &[roots.root]).await?;
    let stored = rebuild(
        &ctx,
        &repo,
        commit,
        TreeScope::NewInCommit,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(stored.get(""), Some(&(roots.root, TreeOutcome::Stored)));
    // Only the root was removed.
    assert_eq!(paths_with(&stored, TreeOutcome::Stored), vec![""]);
    assert_eq!(
        paths_with(&stored, TreeOutcome::Identical),
        vec!["dir", "new"]
    );
    let rebuilt = stored_bytes(&ctx, &repo, roots.root)
        .await?
        .context("the root should be stored")?;
    assert_eq!(rebuilt, saved[0], "the root should have the uploaded bytes");
    let envelope = HgManifestEnvelope::from_blob(BlobstoreBytes::from_bytes(rebuilt).into())?;
    assert_eq!(envelope.node_id(), supplied);
    assert_ne!(envelope.computed_node_id(), supplied);
    Ok(())
}

/// What it tests: a stored Hg tree whose bytes differ from the rebuilt ones is
/// reported and never overwritten.
#[mononoke::fbinit_test]
async fn test_different_stored_tree_is_left_alone(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/a", "a")
        .commit()
        .await?;
    hg_root(&ctx, &repo, root).await?;

    let dir = tree_at(&ctx, &repo, root, "dir").await?;
    let original = HgManifestEnvelope::from_blob(
        BlobstoreBytes::from_bytes(unlink_trees(&ctx, &repo, &[dir]).await?.remove(0)).into(),
    )?;
    let mut altered = original.into_mut();
    altered.contents = Bytes::from_static(b"not the rebuilt contents\n");
    let altered = HgManifestEnvelopeMut::freeze(altered).into_blob();
    repo.repo_blobstore()
        .put(&ctx, dir.blobstore_key(), altered.clone().into())
        .await?;

    let outcomes = rebuild(
        &ctx,
        &repo,
        root,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(outcomes.get("dir"), Some(&(dir, TreeOutcome::Different)));
    assert_eq!(
        stored_bytes(&ctx, &repo, dir).await?,
        Some(BlobstoreBytes::from(altered).into_bytes()),
        "a different stored tree should not be overwritten",
    );
    Ok(())
}

/// What it tests: an augmented manifest whose contents do not hash to its
/// recorded computed node id is reported, and its Hg tree is not stored.
#[mononoke::fbinit_test]
async fn test_inconsistent_augmented_record_is_not_stored(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/a", "a")
        .commit()
        .await?;
    hg_root(&ctx, &repo, root).await?;

    let dir = tree_at(&ctx, &repo, root, "dir").await?;
    let augmented_id = HgAugmentedManifestId::new(dir.into_nodehash());
    let mut augmented = augmented_id.load(&ctx, repo.repo_blobstore()).await?;
    augmented.augmented_manifest.computed_node_id =
        HgNodeHash::new(Sha1::from_byte_array([0xCD; 20]));
    repo.repo_blobstore()
        .unlink(&ctx, &augmented_id.blobstore_key())
        .await?;
    augmented.store(&ctx, repo.repo_blobstore()).await?;
    unlink_trees(&ctx, &repo, &[dir]).await?;

    let outcomes = rebuild(
        &ctx,
        &repo,
        root,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(outcomes.get("dir"), Some(&(dir, TreeOutcome::Inconsistent)));
    assert_eq!(
        stored_bytes(&ctx, &repo, dir).await?,
        None,
        "a tree rebuilt from an inconsistent record should not be stored",
    );
    Ok(())
}

/// What it tests: a commit with no Hg changeset is refused rather than
/// derived.
#[mononoke::fbinit_test]
async fn test_commit_without_hg_changeset_is_refused(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a", "a")
        .commit()
        .await?;

    let err = roots(&ctx, &repo, root)
        .await
        .expect_err("a commit without an Hg changeset should be refused");
    assert!(
        format!("{err:#}").contains("no Hg changeset"),
        "unexpected error: {err:#}",
    );
    assert_eq!(
        repo.bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, root)
            .await?,
        None,
        "nothing should have been derived",
    );
    Ok(())
}

/// What it tests: the null tree, which Hg uses for an empty manifest, has no
/// trees to rebuild, and as a parent every tree differs from it.
#[mononoke::fbinit_test]
async fn test_null_tree_is_empty(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;
    let null = HgManifestId::new(NULL_HASH);

    let rebuilt: Vec<_> = rebuilder(&repo)
        .rebuild(
            &ctx,
            CommitHgRoots {
                root: null,
                parents: vec![],
            },
            TreeScope::Whole,
            RebuildAction::StoreMissing,
        )
        .try_collect()
        .await?;
    assert!(rebuilt.is_empty(), "{rebuilt:?}");

    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/a", "a")
        .commit()
        .await?;
    let root = hg_root(&ctx, &repo, commit).await?;
    let rebuilt: Vec<_> = rebuilder(&repo)
        .rebuild(
            &ctx,
            CommitHgRoots {
                root,
                parents: vec![null],
            },
            TreeScope::NewInCommit,
            RebuildAction::Compare,
        )
        .try_collect()
        .await?;
    let mut paths: Vec<_> = rebuilt.iter().map(|tree| tree.path.to_string()).collect();
    paths.sort();
    assert_eq!(paths, vec!["", "dir"]);
    Ok(())
}

/// The roots of a commit come from the Hg changesets of the commit and of each
/// of its parents.
#[mononoke::fbinit_test]
async fn test_commit_roots_come_from_hg_changesets(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = test_repo_factory::build_empty(fb).await?;

    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a", "a")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("b", "b")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .commit()
        .await?;
    let hg_cs = repo.derive_hg_changeset(&ctx, merge).await?;
    let manifest = HgBlobChangeset::load(&ctx, repo.repo_blobstore(), hg_cs)
        .await?
        .context("the merge's Hg changeset should be stored")?
        .manifestid();

    assert_eq!(
        roots(&ctx, &repo, merge).await?,
        CommitHgRoots {
            root: manifest,
            parents: vec![
                hg_root(&ctx, &repo, p1).await?,
                hg_root(&ctx, &repo, p2).await?
            ],
        },
    );
    Ok(())
}

/// A repo whose only restricted directory is `path`.
async fn repo_restricting(fb: FacebookInit, path: &str) -> Result<Repo> {
    let path = NonRootMPath::new(path)?;
    Ok(test_repo_factory::TestRepoFactory::new(fb)?
        .with_config_override(move |cfg| {
            cfg.restricted_paths_config = RestrictedPathsConfig {
                path_restriction_metadata: [(
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
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            };
        })
        .build()
        .await?)
}

fn restricted_paths_logging_knobs(enabled: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke:enabled_restricted_paths_access_logging".to_string(),
        KnobVal::Bool(enabled),
    )]))
}

async fn hg_restricted_path_entries(
    ctx: &CoreContext,
    repo: &Repo,
) -> Result<Vec<(String, String)>> {
    let mut entries: Vec<_> = repo
        .restricted_paths()
        .config_based()
        .manifest_id_store()
        .get_all_entries(ctx)
        .await?
        .into_iter()
        .filter(|entry| entry.manifest_type == ManifestType::Hg)
        .map(|entry| {
            Ok((
                entry.manifest_id.to_string(),
                entry.repo_path()?.to_string(),
            ))
        })
        .collect::<Result<_>>()?;
    entries.sort();
    Ok(entries)
}

/// What it tests: a stored tree at a restricted directory gets the same Hg
/// restricted-path entry derivation records, so the restored tree is served
/// with its restriction.
#[mononoke::fbinit_test]
async fn test_stored_restricted_tree_records_its_restriction(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo = repo_restricting(fb, "restricted").await?;

    // Derived with restricted-path tracking off, so no Hg entry is recorded,
    // as for commits made once Hg writes stop. Derivation spawns tasks, so the
    // knobs are set for the whole thread.
    override_just_knobs(restricted_paths_logging_knobs(false));
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("restricted/secret", "s")
        .add_file("public/open", "o")
        .commit()
        .await?;
    hg_root(&ctx, &repo, commit).await?;
    let restricted = tree_at(&ctx, &repo, commit, "restricted").await?;
    let public = tree_at(&ctx, &repo, commit, "public").await?;
    assert_eq!(hg_restricted_path_entries(&ctx, &repo).await?, vec![]);
    unlink_trees(&ctx, &repo, &[restricted, public]).await?;

    override_just_knobs(restricted_paths_logging_knobs(true));
    let compared = rebuild(
        &ctx,
        &repo,
        commit,
        TreeScope::Whole,
        RebuildAction::Compare,
    )
    .await?;
    assert_eq!(
        paths_with(&compared, TreeOutcome::Missing),
        vec!["public", "restricted"]
    );
    assert_eq!(
        hg_restricted_path_entries(&ctx, &repo).await?,
        vec![],
        "comparing should record nothing",
    );

    let stored = rebuild(
        &ctx,
        &repo,
        commit,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(
        paths_with(&stored, TreeOutcome::Stored),
        vec!["public", "restricted"]
    );
    assert_eq!(
        hg_restricted_path_entries(&ctx, &repo).await?,
        vec![(
            restricted.to_string(),
            RepoPath::dir("restricted")?.to_string()
        )],
        "only the restricted directory should get an Hg entry",
    );
    Ok(())
}

/// What it tests: storing records the restriction of a restricted directory
/// whose tree is already stored, as when a public directory with the same
/// contents, and so the same tree id, was stored first.
#[mononoke::fbinit_test]
async fn test_already_stored_restricted_tree_records_its_restriction(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo = repo_restricting(fb, "restricted").await?;

    override_just_knobs(restricted_paths_logging_knobs(false));
    let public = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("copy/f", "same")
        .commit()
        .await?;
    let restricted = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("restricted/f", "same")
        .commit()
        .await?;
    hg_root(&ctx, &repo, public).await?;
    hg_root(&ctx, &repo, restricted).await?;
    let shared = tree_at(&ctx, &repo, public, "copy").await?;
    assert_eq!(
        tree_at(&ctx, &repo, restricted, "restricted").await?,
        shared,
        "fixture: both directories should have the same tree id",
    );
    unlink_trees(&ctx, &repo, &[shared]).await?;

    override_just_knobs(restricted_paths_logging_knobs(true));
    let stored = rebuild(
        &ctx,
        &repo,
        public,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(stored.get("copy"), Some(&(shared, TreeOutcome::Stored)));
    assert_eq!(hg_restricted_path_entries(&ctx, &repo).await?, vec![]);

    let compared = rebuild(
        &ctx,
        &repo,
        restricted,
        TreeScope::Whole,
        RebuildAction::Compare,
    )
    .await?;
    assert_eq!(
        compared.get("restricted"),
        Some(&(shared, TreeOutcome::Identical))
    );
    assert_eq!(
        hg_restricted_path_entries(&ctx, &repo).await?,
        vec![],
        "comparing should record nothing",
    );

    let stored = rebuild(
        &ctx,
        &repo,
        restricted,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(
        stored.get("restricted"),
        Some(&(shared, TreeOutcome::Identical))
    );
    assert_eq!(
        hg_restricted_path_entries(&ctx, &repo).await?,
        vec![(shared.to_string(), RepoPath::dir("restricted")?.to_string())],
    );
    Ok(())
}

/// What it tests: storing records the restriction of a restricted directory
/// whose stored tree differs, because the entry names the tree id and path,
/// not the bytes, while the differing tree is left alone.
#[mononoke::fbinit_test]
async fn test_different_restricted_tree_records_its_restriction(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo = repo_restricting(fb, "restricted").await?;

    override_just_knobs(restricted_paths_logging_knobs(false));
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("restricted/secret", "s")
        .commit()
        .await?;
    hg_root(&ctx, &repo, commit).await?;
    let restricted = tree_at(&ctx, &repo, commit, "restricted").await?;
    let original = HgManifestEnvelope::from_blob(
        BlobstoreBytes::from_bytes(unlink_trees(&ctx, &repo, &[restricted]).await?.remove(0))
            .into(),
    )?;
    let mut altered = original.into_mut();
    altered.contents = Bytes::from_static(b"not the rebuilt contents\n");
    repo.repo_blobstore()
        .put(
            &ctx,
            restricted.blobstore_key(),
            HgManifestEnvelopeMut::freeze(altered).into_blob().into(),
        )
        .await?;

    override_just_knobs(restricted_paths_logging_knobs(true));
    let stored = rebuild(
        &ctx,
        &repo,
        commit,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(
        stored.get("restricted"),
        Some(&(restricted, TreeOutcome::Different))
    );
    assert_eq!(
        hg_restricted_path_entries(&ctx, &repo).await?,
        vec![(
            restricted.to_string(),
            RepoPath::dir("restricted")?.to_string()
        )],
    );
    Ok(())
}

/// What it tests: with restricted-path recording switched off, a missing tree
/// at a restricted directory is not stored, because its restriction could not
/// be recorded, while other missing trees are; a rerun with recording back on
/// stores it with its restriction.
#[mononoke::fbinit_test]
async fn test_restricted_tree_is_not_stored_while_recording_is_off(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo = repo_restricting(fb, "restricted").await?;

    override_just_knobs(restricted_paths_logging_knobs(false));
    let commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("restricted/secret", "s")
        .add_file("public/open", "o")
        .commit()
        .await?;
    hg_root(&ctx, &repo, commit).await?;
    let restricted = tree_at(&ctx, &repo, commit, "restricted").await?;
    let public = tree_at(&ctx, &repo, commit, "public").await?;
    unlink_trees(&ctx, &repo, &[restricted, public]).await?;

    let stored = rebuild(
        &ctx,
        &repo,
        commit,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(
        stored.get("restricted"),
        Some(&(restricted, TreeOutcome::RestrictionNotRecorded))
    );
    assert_eq!(stored.get("public"), Some(&(public, TreeOutcome::Stored)));
    assert_eq!(
        stored_bytes(&ctx, &repo, restricted).await?,
        None,
        "a restricted tree should not be stored without its restriction",
    );
    assert_eq!(hg_restricted_path_entries(&ctx, &repo).await?, vec![]);

    override_just_knobs(restricted_paths_logging_knobs(true));
    let stored = rebuild(
        &ctx,
        &repo,
        commit,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(
        stored.get("restricted"),
        Some(&(restricted, TreeOutcome::Stored))
    );
    assert_eq!(
        hg_restricted_path_entries(&ctx, &repo).await?,
        vec![(
            restricted.to_string(),
            RepoPath::dir("restricted")?.to_string()
        )],
    );
    Ok(())
}

/// What it tests: with restricted-path recording switched off, a restricted
/// directory whose tree is already stored, here by a public directory with the
/// same contents and so the same tree id, is reported as not recorded, so the
/// command fails and a rerun with recording on records its restriction.
#[mononoke::fbinit_test]
async fn test_shared_restricted_tree_is_reported_while_recording_is_off(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo = repo_restricting(fb, "restricted").await?;

    override_just_knobs(restricted_paths_logging_knobs(false));
    let public = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("copy/f", "same")
        .commit()
        .await?;
    let restricted = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("restricted/f", "same")
        .commit()
        .await?;
    hg_root(&ctx, &repo, public).await?;
    hg_root(&ctx, &repo, restricted).await?;
    let shared = tree_at(&ctx, &repo, public, "copy").await?;
    assert_eq!(
        tree_at(&ctx, &repo, restricted, "restricted").await?,
        shared,
        "fixture: both directories should have the same tree id",
    );
    unlink_trees(&ctx, &repo, &[shared]).await?;

    let stored = rebuild(
        &ctx,
        &repo,
        public,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(stored.get("copy"), Some(&(shared, TreeOutcome::Stored)));
    let stored = rebuild(
        &ctx,
        &repo,
        restricted,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(
        stored.get("restricted"),
        Some(&(shared, TreeOutcome::RestrictionNotRecorded)),
        "a restricted directory whose restriction could not be recorded should be reported",
    );
    assert_eq!(hg_restricted_path_entries(&ctx, &repo).await?, vec![]);

    override_just_knobs(restricted_paths_logging_knobs(true));
    let stored = rebuild(
        &ctx,
        &repo,
        restricted,
        TreeScope::Whole,
        RebuildAction::StoreMissing,
    )
    .await?;
    assert_eq!(
        stored.get("restricted"),
        Some(&(shared, TreeOutcome::Identical))
    );
    assert_eq!(
        hg_restricted_path_entries(&ctx, &repo).await?,
        vec![(shared.to_string(), RepoPath::dir("restricted")?.to_string())],
    );
    Ok(())
}
