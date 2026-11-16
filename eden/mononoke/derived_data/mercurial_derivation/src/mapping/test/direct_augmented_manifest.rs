/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

async fn load_aug_envelope(
    ctx: &CoreContext,
    repo: &TestRepo,
    aug: &RootHgAugmentedManifestId,
) -> Result<HgAugmentedManifestEnvelope> {
    Loadable::load(&aug.hg_augmented_manifest_id(), ctx, repo.repo_blobstore())
        .await
        .with_context(|| {
            format!(
                "Failed to load augmented manifest envelope for {}",
                aug.hg_augmented_manifest_id(),
            )
        })
}

fn assert_aug_envelopes_match(
    old: &HgAugmentedManifestEnvelope,
    new: &HgAugmentedManifestEnvelope,
    cs_id: ChangesetId,
) {
    assert_eq!(
        old.augmented_manifest_id, new.augmented_manifest_id,
        "Blake3 digest mismatch for {cs_id}"
    );
    assert_eq!(
        old.augmented_manifest_size, new.augmented_manifest_size,
        "Size mismatch for {cs_id}"
    );
    assert_eq!(
        old.augmented_manifest.hg_node_id, new.augmented_manifest.hg_node_id,
        "hg_node_id mismatch for {cs_id}"
    );
    assert_eq!(
        old.augmented_manifest.computed_node_id, new.augmented_manifest.computed_node_id,
        "computed_node_id mismatch for {cs_id}"
    );
    assert_eq!(
        old.augmented_manifest.p1, new.augmented_manifest.p1,
        "p1 mismatch for {cs_id}"
    );
    assert_eq!(
        old.augmented_manifest.p2, new.augmented_manifest.p2,
        "p2 mismatch for {cs_id}"
    );
}

async fn create_two_commit_repo(fb: FacebookInit) -> Result<(TestRepo, ChangesetId, ChangesetId)> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a.txt", "initial")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("a.txt", "modified")
        .add_file("b.txt", "new file")
        .commit()
        .await?;
    Ok((repo, root, child))
}

async fn derive_old_path_augmented_manifest_envelopes(
    ctx: &CoreContext,
    repo: &TestRepo,
    csids: Vec<ChangesetId>,
) -> Result<HashMap<ChangesetId, HgAugmentedManifestEnvelope>> {
    let manager = repo.repo_derived_data().manager();
    fixtures::store_hg_changesets_as_uploaded(ctx, repo, csids.clone()).await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(ctx, csids.clone(), None)
        .await?;
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(ctx, csids.clone(), None)
        .await?;
    let roots = manager
        .fetch_derived_batch::<RootHgAugmentedManifestId>(ctx, csids.clone(), None)
        .await?;
    let mut envelopes = HashMap::new();
    for cs_id in csids {
        let aug = roots
            .get(&cs_id)
            .with_context(|| format!("Missing old-path RootHgAugmentedManifestId for {cs_id}"))?;
        envelopes.insert(cs_id, load_aug_envelope(ctx, repo, aug).await?);
    }
    Ok(envelopes)
}

#[mononoke::fbinit_test]
async fn test_augmented_manifest_manager_derivation_does_not_require_hgchangesets(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);

    // Given: a linear history with only the non-HgChangeset dependency prederived.
    let (repo, root, child) = create_two_commit_repo(fb).await?;
    let csids = vec![root, child];
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
        .await?;

    // When: deriving through the manager.
    manager
        .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, csids.clone(), None)
        .await?;

    // Then: roots are stored and no HgChangeset mappings were created as a side effect.
    let derived = manager
        .fetch_derived_batch::<RootHgAugmentedManifestId>(&ctx, csids.clone(), None)
        .await?;
    for cs_id in csids {
        let aug = derived
            .get(&cs_id)
            .with_context(|| format!("Missing RootHgAugmentedManifestId for {cs_id}"))?;
        let env = load_aug_envelope(&ctx, &repo, aug).await?;
        assert_eq!(
            env.augmented_manifest.hg_node_id, env.augmented_manifest.computed_node_id,
            "derivation should use a content-derived root when no canonical Hg mapping exists for {cs_id}",
        );
        assert!(
            manager
                .fetch_derived::<MappedHgChangesetId>(&ctx, cs_id, None)
                .await?
                .is_none(),
            "manager derivation must not create MappedHgChangesetId for {cs_id}",
        );
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_augmented_manifest_derive_single_without_hgchangeset_mapping_matches_old_path(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);

    // Given: an old-path baseline and an equivalent repo where only the
    // non-HgChangeset dependency is prederived.
    let (baseline_repo, root, child) = create_two_commit_repo(fb).await?;
    let csids = vec![root, child];
    let baseline_envs =
        derive_old_path_augmented_manifest_envelopes(&ctx, &baseline_repo, csids.clone()).await?;

    let (repo, root, child) = create_two_commit_repo(fb).await?;
    assert_eq!(csids, vec![root, child], "test commits should be stable");
    let manager = repo.repo_derived_data().manager();
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, csids.clone(), None)
        .await?;
    for cs_id in &csids {
        assert!(
            manager
                .fetch_derived::<MappedHgChangesetId>(&ctx, *cs_id, None)
                .await?
                .is_none(),
            "direct single-derive fixture should not prederive MappedHgChangesetId for {cs_id}",
        );
    }
    let derivation_ctx = manager.derivation_context(None);
    let root_bonsai = Loadable::load(&root, &ctx, repo.repo_blobstore()).await?;
    let child_bonsai = Loadable::load(&child, &ctx, repo.repo_blobstore()).await?;

    // When: deriving the root and child through the single-derive path,
    // without a mapped HgChangeset or persisted HgManifest.
    let root_aug =
        RootHgAugmentedManifestId::derive_single(&ctx, &derivation_ctx, root_bonsai, vec![], None)
            .await?;
    let child_aug = RootHgAugmentedManifestId::derive_single(
        &ctx,
        &derivation_ctx,
        child_bonsai,
        vec![root_aug.clone()],
        None,
    )
    .await?;

    // Then: single derivation matches the old path and does not create
    // HgChangeset mappings or raw HgManifest blobs.
    for (cs_id, aug) in [(root, root_aug), (child, child_aug)] {
        let env = load_aug_envelope(&ctx, &repo, &aug).await?;
        let baseline = baseline_envs
            .get(&cs_id)
            .with_context(|| format!("Missing baseline envelope for {cs_id}"))?;
        assert_aug_envelopes_match(baseline, &env, cs_id);
        assert_hgmanifest_blob_absent(
            &ctx,
            &repo,
            HgManifestId::new(env.augmented_manifest.hg_node_id),
            cs_id,
        )
        .await?;
        assert!(
            manager
                .fetch_derived::<MappedHgChangesetId>(&ctx, cs_id, None)
                .await?
                .is_none(),
            "direct single derivation must not create MappedHgChangesetId for {cs_id}",
        );
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_augmented_manifest_derive_single_uses_mapped_hg_root_when_available(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, root, _) = create_two_commit_repo(fb).await?;
    let manager = repo.repo_derived_data().manager();

    // Given: the canonical Hg root is available through the Bonsai-Hg
    // mapping, as if a client uploaded a canonical Hg manifest.
    fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, vec![root]).await?;
    manager
        .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![root], None)
        .await?;
    let mapped_hg = manager
        .fetch_derived::<MappedHgChangesetId>(&ctx, root, None)
        .await?
        .with_context(|| format!("Missing MappedHgChangesetId for {root}"))?;
    let expected_root = Loadable::load(&mapped_hg.hg_changeset_id(), &ctx, repo.repo_blobstore())
        .await?
        .manifestid()
        .into_nodehash();
    let derivation_ctx = manager.derivation_context(None);
    let bonsai = Loadable::load(&root, &ctx, repo.repo_blobstore()).await?;

    // When: deriving through the single-derive path.
    let aug = RootHgAugmentedManifestId::derive_single(&ctx, &derivation_ctx, bonsai, vec![], None)
        .await?;

    // Then: derivation preserves the mapped canonical Hg root.
    let env = load_aug_envelope(&ctx, &repo, &aug).await?;
    assert_eq!(
        env.augmented_manifest.hg_node_id, expected_root,
        "direct derivation should use the mapped canonical Hg root",
    );

    Ok(())
}
