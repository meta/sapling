/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_stream::try_stream;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivationContext;
use futures::StreamExt;
use manifest::Entry;
use manifest::walk_added;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::audit::CommitInfo;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::StoredObject;
use mononoke_types::audit::WalkStream;
use mononoke_types::audit::Walkable;
use repo_derived_data::RepoDerivedDataRef;
use repo_derived_data::audit::Mapping;
use repo_derived_data::audit::MappingKey;

use crate::RootUnodeManifestId;
use crate::format_key;

impl MappingKey for RootUnodeManifestId {
    fn mapping_key(
        derivation_ctx: &DerivationContext,
        changeset_id: ChangesetId,
    ) -> Option<String> {
        Some(format_key(derivation_ctx, changeset_id))
    }
}

/// The unode family: the commit's mapping, then every manifest and file unode
/// that differs from all parents' unodes at the same path. File contents
/// belong to `core`.
impl<R> Walkable<R> for RootUnodeManifestId
where
    R: RepoDerivedDataRef + Send + Sync + 'static,
{
    const NAME: &'static str = "unode";
    const DERIVED_TYPE: Option<DerivableType> = Some(Self::VARIANT);

    fn walk_new(
        ctx: LoadContext,
        repo: Arc<R>,
        commit: Arc<CommitInfo>,
        max_loads: NonZeroUsize,
    ) -> WalkStream<R> {
        try_stream! {
            let root = Mapping::<Self>::load_own(&ctx, &*repo, &commit).await?;
            let root_id = *root.manifest_unode_id();
            yield StoredObject::preloaded(Mapping::<Self>::new(commit.changeset_id), root);

            let parent_roots = Mapping::<Self>::load_parents(&ctx, &*repo, &commit)
                .await?
                .iter()
                .map(|parent| *parent.manifest_unode_id())
                .collect();

            let added = walk_added(
                ctx.clone(),
                repo.clone(),
                ctx.blobstore().clone(),
                root_id,
                parent_roots,
                max_loads,
            );
            for await entry in added {
                yield match entry? {
                    Entry::Tree(manifest) => manifest,
                    Entry::Leaf((_path, file)) => StoredObject::new(file),
                };
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use anyhow::Context;
    use anyhow::Result;
    use blobstore::KeyedBlobstore;
    use context::CoreContext;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use manifest::Entry;
    use manifest::ManifestOps;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use mononoke_types::BlobstoreKey;
    use mononoke_types::ChangesetId;
    use mononoke_types::MPath;
    use mononoke_types::ManifestUnodeId;
    use mononoke_types::audit::ObjectId;
    use mononoke_types::audit::WalkStage;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_derived_data::audit::Mapping;
    use test_repo_factory::TestRepoFactory;
    use test_repo_factory::build_empty;
    use tests_utils::BasicTestRepo;
    use tests_utils::CreateCommitContext;
    use tests_utils::audit::OverlappingReadsBlobstore;
    use tests_utils::audit::failed;
    use tests_utils::audit::load_ctx;
    use tests_utils::audit::walk;

    use crate::RootUnodeManifestId;

    type Repo = BasicTestRepo;

    async fn derive(
        ctx: &CoreContext,
        repo: &Repo,
        changeset_id: ChangesetId,
    ) -> Result<ManifestUnodeId> {
        let root = repo
            .repo_derived_data()
            .derive::<RootUnodeManifestId>(ctx, changeset_id, DerivationPriority::LOW)
            .await?;
        Ok(*root.manifest_unode_id())
    }

    /// Every manifest and file unode in a tree, listed from the stored nodes.
    async fn tree(
        ctx: &CoreContext,
        repo: &Repo,
        root: ManifestUnodeId,
    ) -> Result<HashSet<ObjectId>> {
        root.list_all_entries(ctx.clone(), repo.repo_blobstore().clone())
            .map_ok(|(_path, entry)| match entry {
                Entry::Tree(directory) => ObjectId::of::<Repo, _>(&directory),
                Entry::Leaf(file) => ObjectId::of::<Repo, _>(&file),
            })
            .try_collect()
            .await
    }

    /// Unodes in the commit's tree that are in none of its parents' trees.
    async fn added(
        ctx: &CoreContext,
        repo: &Repo,
        root: ManifestUnodeId,
        parent_roots: &[ManifestUnodeId],
    ) -> Result<HashSet<ObjectId>> {
        let mut objects = tree(ctx, repo, root).await?;
        for parent in parent_roots {
            for object in tree(ctx, repo, *parent).await? {
                objects.remove(&object);
            }
        }
        Ok(objects)
    }

    /// Walks the family, checks that the commit's mapping comes first and that
    /// every object loads, and returns the distinct unodes after the mapping.
    async fn walk_unodes(
        ctx: &CoreContext,
        repo: &Arc<Repo>,
        changeset_id: ChangesetId,
    ) -> Result<HashSet<ObjectId>> {
        let objects = walk::<RootUnodeManifestId, _>(ctx, repo, changeset_id, 4).await??;
        assert_eq!(
            objects.first().map(|object| object.id().clone()),
            Some(ObjectId::of::<Repo, _>(
                &Mapping::<RootUnodeManifestId>::new(changeset_id)
            )),
        );
        let ids = objects
            .iter()
            .skip(1)
            .map(|object| object.id().clone())
            .collect::<Vec<_>>();
        for object in objects {
            object
                .validate(&load_ctx(ctx, &**repo), &**repo, false)
                .await?;
        }
        let unique = ids.iter().cloned().collect::<HashSet<_>>();
        assert_eq!(unique.len(), ids.len(), "duplicate objects in {ids:?}");
        Ok(unique)
    }

    #[mononoke::fbinit_test]
    async fn each_commit_emits_unodes_new_relative_to_all_parents(fb: FacebookInit) -> Result<()> {
        // Given a root, two branches that change different files, and a merge
        // that keeps both changes
        let ctx = CoreContext::test_mock(fb);
        let repo = Arc::new(build_empty::<Repo>(fb).await?);
        let root = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("dir1/file1", "1")
            .add_file("dir2/file2", "2")
            .commit()
            .await?;
        let left = CreateCommitContext::new(&ctx, &*repo, vec![root])
            .add_file("dir1/file1", "left")
            .commit()
            .await?;
        let right = CreateCommitContext::new(&ctx, &*repo, vec![root])
            .add_file("dir2/file2", "right")
            .commit()
            .await?;
        let merge = CreateCommitContext::new(&ctx, &*repo, vec![left, right])
            .add_file("dir1/file1", "left")
            .add_file("dir2/file2", "right")
            .commit()
            .await?;
        let root_unode = derive(&ctx, &repo, root).await?;
        let left_unode = derive(&ctx, &repo, left).await?;
        let right_unode = derive(&ctx, &repo, right).await?;
        let merge_unode = derive(&ctx, &repo, merge).await?;

        for (commit, commit_unode, parent_unodes) in [
            (root, root_unode, vec![]),
            (left, left_unode, vec![root_unode]),
            (right, right_unode, vec![root_unode]),
            (merge, merge_unode, vec![left_unode, right_unode]),
        ] {
            // When the commit's additions are walked
            let walked = walk_unodes(&ctx, &repo, commit).await?;

            // Then they are exactly the unodes in its tree and in no parent's tree
            let expected = added(&ctx, &repo, commit_unode, &parent_unodes).await?;
            assert_eq!(walked, expected, "commit {commit}");
        }
        // The merge reuses `dir1` from the left and `dir2` from the right, so a
        // diff against the first parent alone would also emit the right's `dir2`.
        assert_eq!(
            added(&ctx, &repo, merge_unode, &[left_unode, right_unode]).await?,
            HashSet::from([ObjectId::of::<Repo, _>(&merge_unode)])
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn missing_manifest_fails_by_key_against_the_commit_that_added_it(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given A adds `dir/sub`, B changes a file in it, and A's `dir/sub`
        // manifest unode is removed
        let ctx = CoreContext::test_mock(fb);
        let repo = Arc::new(build_empty::<Repo>(fb).await?);
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("dir/sub/file", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .add_file("dir/sub/file", "b")
            .commit()
            .await?;
        let root_a = derive(&ctx, &repo, a).await?;
        derive(&ctx, &repo, b).await?;
        let sub_a = root_a
            .find_entry(
                ctx.clone(),
                repo.repo_blobstore().clone(),
                MPath::new("dir/sub")?,
            )
            .await?
            .and_then(Entry::into_tree)
            .context("A has no directory dir/sub")?;
        repo.repo_blobstore()
            .unlink(&ctx, &sub_a.blobstore_key())
            .await?;

        // When A's and B's additions are walked
        let error_a = walk::<RootUnodeManifestId, _>(&ctx, &repo, a, 4)
            .await?
            .expect_err("A's dir/sub manifest unode was removed");
        let error_b = walk::<RootUnodeManifestId, _>(&ctx, &repo, b, 4)
            .await?
            .expect_err("B's parent's dir/sub manifest unode was removed");

        // Then A, which added it, fails to load it, and B fails to read it as
        // its parent's directory; both name it
        let sub_a = Some(ObjectId::of::<Repo, _>(&sub_a));
        assert_eq!(failed(error_a), ("missing", WalkStage::Load, sub_a.clone()));
        assert_eq!(failed(error_b), ("missing", WalkStage::Enumerate, sub_a));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn one_commit_reads_up_to_its_limit_of_manifests_at_once(fb: FacebookInit) -> Result<()> {
        // Given a root commit with eight directories, in a repository that
        // records overlapping manifest reads
        let ctx = CoreContext::test_mock(fb);
        let (blobstore, peak) = OverlappingReadsBlobstore::new(Memblob::default(), "manifestunode");
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(blobstore))
            .build()
            .await?;
        let repo = Arc::new(repo);
        let a = (0..8)
            .fold(CreateCommitContext::new_root(&ctx, &*repo), |commit, n| {
                commit.add_file(format!("dir{n}/file").as_str(), "a")
            })
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        peak.store(0, Ordering::SeqCst);

        // When its additions are walked with a limit of four loads
        let objects = walk::<RootUnodeManifestId, _>(&ctx, &repo, a, 4).await??;

        // Then it emits the root and the eight directories, and reads four
        // directories at once
        assert_eq!(
            objects
                .iter()
                .filter(|object| object.id().name() == "UnodeManifest")
                .count(),
            9
        );
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        Ok(())
    }
}
