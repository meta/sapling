/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;

use anyhow::Result;
use blobstore::Loadable;
use context::CoreContext;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreKey;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::ObjectId;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use tests_utils::CreateCommitContext;
use tests_utils::audit::load_ctx;
use tests_utils::audit::walk;

use crate::test_utils::TestRepo;
use crate::test_utils::test_repo;

#[mononoke::fbinit_test]
async fn core_emits_changeset_and_new_file_contents(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, _blobstore) = test_repo(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &*repo)
        .add_file("a", "one")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &*repo, vec![root])
        .add_file("b", "two")
        .add_file("c", "three")
        .delete_file("a")
        .commit()
        .await?;
    let changeset = Loadable::load(&child, &ctx, repo.repo_blobstore()).await?;
    let contents = changeset
        .simplified_file_changes()
        .filter_map(|(_path, change)| Some(change?.content_id()))
        .collect::<Vec<_>>();
    assert_eq!(contents.len(), 2);

    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, child, 4).await??;

    // Content metadata and aliases are covered by the `content` tests.
    let ids = objects
        .iter()
        .map(|object| object.id().clone())
        .filter(|id| matches!(id.name(), "Changeset" | "FileContent"))
        .collect::<Vec<_>>();
    let expected = [ObjectId::of::<TestRepo, _>(&child)]
        .into_iter()
        .chain(contents.iter().map(ObjectId::of::<TestRepo, ContentId>))
        .collect::<Vec<_>>();
    assert_eq!(
        ids.iter().collect::<HashSet<_>>(),
        expected.iter().collect::<HashSet<_>>()
    );
    assert_eq!(ids[0], expected[0]);
    assert_eq!(ids[0].to_string(), format!("Changeset:{child}"));
    for object in objects {
        assert_eq!(
            object
                .validate(&load_ctx(&ctx, &*repo), &*repo, true)
                .await?,
            vec![]
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn core_emits_a_content_added_at_several_paths_once(fb: FacebookInit) -> Result<()> {
    // Given a commit that adds the same content at two paths
    let ctx = CoreContext::test_mock(fb);
    let (repo, _blobstore) = test_repo(fb).await?;
    let commit = CreateCommitContext::new_root(&ctx, &*repo)
        .add_file("a", "same")
        .add_file("copy/a", "same")
        .commit()
        .await?;
    let changeset = Loadable::load(&commit, &ctx, repo.repo_blobstore()).await?;
    let contents = changeset
        .simplified_file_changes()
        .filter_map(|(_path, change)| Some(change?.content_id()))
        .collect::<HashSet<_>>();
    assert_eq!(contents.len(), 1, "both paths hold one content");

    // When the core family walks it
    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4).await??;

    // Then the content, its metadata and each alias are emitted, and read,
    // once
    let ids = objects
        .iter()
        .map(|object| object.id().clone())
        .collect::<Vec<_>>();
    let content = ObjectId::of::<TestRepo, ContentId>(contents.iter().next().expect("one content"));
    assert_eq!(ids.iter().filter(|id| **id == content).count(), 1);
    assert_eq!(
        ids.iter().collect::<HashSet<_>>().len(),
        ids.len(),
        "an object was emitted twice: {ids:?}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn core_emits_merge_contents_a_parent_holds(fb: FacebookInit) -> Result<()> {
    // Given a merge that keeps the left parent's `file1` and the right
    // parent's `file2`
    let ctx = CoreContext::test_mock(fb);
    let (repo, _blobstore) = test_repo(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &*repo)
        .add_file("file1", "root")
        .add_file("file2", "root")
        .commit()
        .await?;
    let left = CreateCommitContext::new(&ctx, &*repo, vec![root])
        .add_file("file1", "left")
        .commit()
        .await?;
    let right = CreateCommitContext::new(&ctx, &*repo, vec![root])
        .add_file("file2", "right")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &*repo, vec![left, right])
        .add_file("file1", "left")
        .add_file("file2", "right")
        .commit()
        .await?;
    let mut parent_contents = Vec::new();
    for parent in [left, right] {
        let changeset = Loadable::load(&parent, &ctx, repo.repo_blobstore()).await?;
        parent_contents.extend(
            changeset
                .simplified_file_changes()
                .filter_map(|(_path, change)| Some(change?.content_id())),
        );
    }

    // When the merge's core family is walked
    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, merge, 4).await??;

    // Then it emits the merge's changeset and both parents' contents again
    let expected = [ObjectId::of::<TestRepo, _>(&merge)]
        .into_iter()
        .chain(
            parent_contents
                .iter()
                .map(ObjectId::of::<TestRepo, ContentId>),
        )
        .collect::<HashSet<_>>();
    assert_eq!(expected.len(), 3);
    // Content metadata and aliases are covered by the `content` tests.
    assert_eq!(
        objects
            .iter()
            .map(|object| object.id().clone())
            .filter(|id| matches!(id.name(), "Changeset" | "FileContent"))
            .collect::<HashSet<_>>(),
        expected
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn missing_file_content_fails_validation(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let root = CreateCommitContext::new_root(&ctx, &*repo)
        .add_file("a", "one")
        .commit()
        .await?;
    let changeset = Loadable::load(&root, &ctx, repo.repo_blobstore()).await?;
    let content_id = changeset
        .simplified_file_changes()
        .find_map(|(_path, change)| Some(change?.content_id()))
        .expect("root commit adds one file");
    let content = walk::<BonsaiChangeset, _>(&ctx, &repo, root, 4)
        .await??
        .into_iter()
        .find(|object| *object.id() == ObjectId::of::<TestRepo, _>(&content_id))
        .expect("core family emits the added content");
    let key = format!(
        "{}{}",
        repo.repo_identity().id().prefix(),
        content_id.blobstore_key()
    );
    assert_eq!(blobstore.unlink(key).await?, Some(()));

    let error = content
        .validate(&load_ctx(&ctx, &*repo), &*repo, false)
        .await
        .expect_err("content blob was removed");

    assert!(matches!(error, LoadError::Missing(_)), "{error:?}");
    Ok(())
}

#[mononoke::test]
fn object_ids_of_different_types_are_distinct() -> Result<()> {
    let changeset = ChangesetId::from_bytes([7; 32])?;
    let content = ContentId::from_bytes([7; 32])?;

    let changeset_id = ObjectId::of::<TestRepo, _>(&changeset);
    let content_id = ObjectId::of::<TestRepo, _>(&content);

    assert_ne!(changeset_id, content_id);
    assert_eq!(changeset_id, ObjectId::of::<TestRepo, _>(&changeset));
    assert_eq!(
        HashSet::from([changeset_id.clone(), changeset_id, content_id]).len(),
        2
    );
    Ok(())
}
