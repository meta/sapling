/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use arc_swap::ArcSwap;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bytes::Bytes;
use chrono::FixedOffset;
use chrono::TimeZone;
use context::PerfCounterType;
use cross_repo_sync::CommitSyncData;
use cross_repo_sync::CommitSyncRepos;
use cross_repo_sync::SubmoduleDeps;
use cross_repo_sync::test_utils::init_small_large_repo;
use cross_repo_sync::update_mapping_with_version;
use fbinit::FacebookInit;
use fixtures::BranchUneven;
use fixtures::Linear;
use fixtures::ManyFilesDirs;
use fixtures::TestRepoFixture;
use futures::FutureExt;
use futures::future::BoxFuture;
use live_commit_sync_config::TestLiveCommitSyncConfigSource;
use maplit::hashmap;
use metaconfig_types::CommitSyncConfigVersion;
use metaconfig_types::CommitSyncDirection;
use metaconfig_types::DefaultSmallToLargeCommitSyncPathAction;
use mononoke_macros::mononoke;
use mononoke_repos::MononokeRepos;
use mononoke_repos::RepoLoader;
use mononoke_types::BlobstoreKey;
use mononoke_types::NonRootMPath;
use mononoke_types::content_manifest::ContentManifestEntry;
use mononoke_types::hash::Blake3;
use mononoke_types::hash::GitSha1;
use mononoke_types::hash::RichGitSha1;
use mononoke_types::hash::Sha1;
use mononoke_types::hash::Sha256;
use redactedblobstore::RedactedBlobs;
use redactedblobstore::RedactedMetadata;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;
use tests_utils::resolve_cs_id;
use tracing::info;

use crate::BookmarkFreshness;
use crate::ChangesetId;
use crate::ChangesetIdPrefix;
use crate::ChangesetPrefixSpecifier;
use crate::ChangesetSpecifier;
use crate::ChangesetSpecifierPrefixResolution;
use crate::CoreContext;
use crate::FileId;
use crate::FileMetadata;
use crate::FileType;
use crate::HgChangesetId;
use crate::HgChangesetIdPrefix;
use crate::Mononoke;
use crate::Repo;
use crate::XRepoLookupSyncBehaviour;
use crate::repo::XRepoLookupExactBehaviour;

#[mononoke::fbinit_test]
async fn commit_info_by_hash(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke =
        Mononoke::new_test(vec![("test".to_string(), Linear::get_repo(fb).await)]).await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let hash = "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6";
    let cs_id = ChangesetId::from_str(hash)?;
    let cs = repo.changeset(cs_id).await?.expect("changeset exists");

    assert_eq!(cs.message().await?, "modified 10");
    assert_eq!(cs.author().await?, "Jeremy Fitzhardinge <jsgf@fb.com>");
    assert_eq!(
        cs.author_date().await?,
        FixedOffset::west_opt(7 * 3600)
            .unwrap()
            .timestamp_opt(1504041761, 0)
            .unwrap()
    );
    assert_eq!(cs.generation().await?.value(), 11);

    Ok(())
}

#[mononoke::fbinit_test]
async fn commit_info_by_hg_hash(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke =
        Mononoke::new_test(vec![("test".to_string(), Linear::get_repo(fb).await)]).await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let hg_hash = "607314ef579bd2407752361ba1b0c1729d08b281";
    let hg_cs_id = HgChangesetId::from_str(hg_hash)?;
    let cs = repo.changeset(hg_cs_id).await?.expect("changeset exists");

    let hash = "2cb6d2d3052bfbdd6a95a61f2816d81130033b5f5a99e8d8fc24d9238d85bb48";
    assert_eq!(cs.id(), ChangesetId::from_str(hash)?);
    assert_eq!(cs.hg_id().await?, Some(HgChangesetId::from_str(hg_hash)?));
    assert_eq!(cs.message().await?, "added 3");
    assert_eq!(cs.author().await?, "Jeremy Fitzhardinge <jsgf@fb.com>");
    assert_eq!(
        cs.author_date().await?,
        FixedOffset::west_opt(7 * 3600)
            .unwrap()
            .timestamp_opt(1504041758, 0)
            .unwrap()
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn commit_info_by_bookmark(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke =
        Mononoke::new_test(vec![("test".to_string(), Linear::get_repo(fb).await)]).await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let cs = repo
        .resolve_bookmark(&BookmarkKey::new("master")?, BookmarkFreshness::MostRecent)
        .await?
        .expect("bookmark exists");

    let hash = "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6";
    assert_eq!(cs.id(), ChangesetId::from_str(hash)?);
    let hg_hash = "79a13814c5ce7330173ec04d279bf95ab3f652fb";
    assert_eq!(cs.hg_id().await?, Some(HgChangesetId::from_str(hg_hash)?));
    assert_eq!(cs.message().await?, "modified 10");
    assert_eq!(cs.author().await?, "Jeremy Fitzhardinge <jsgf@fb.com>");
    assert_eq!(
        cs.author_date().await?,
        FixedOffset::west_opt(7 * 3600)
            .unwrap()
            .timestamp_opt(1504041761, 0)
            .unwrap()
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn commit_hg_changeset_ids(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke =
        Mononoke::new_test(vec![("test".to_string(), Linear::get_repo(fb).await)]).await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let hash1 = "2cb6d2d3052bfbdd6a95a61f2816d81130033b5f5a99e8d8fc24d9238d85bb48";
    let hash2 = "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6";
    let hg_hash1 = "607314ef579bd2407752361ba1b0c1729d08b281";
    let hg_hash2 = "79a13814c5ce7330173ec04d279bf95ab3f652fb";
    let ids: HashMap<_, _> = repo
        .many_changeset_hg_ids(vec![
            ChangesetId::from_str(hash1)?,
            ChangesetId::from_str(hash2)?,
        ])
        .await?
        .into_iter()
        .collect();
    assert_eq!(
        ids.get(&ChangesetId::from_str(hash1)?),
        Some(&HgChangesetId::from_str(hg_hash1)?)
    );
    assert_eq!(
        ids.get(&ChangesetId::from_str(hash2)?),
        Some(&HgChangesetId::from_str(hg_hash2)?)
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn commit_is_ancestor_of(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke =
        Mononoke::new_test(vec![("test".to_string(), BranchUneven::get_repo(fb).await)]).await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let mut changesets = Vec::new();
    for hg_hash in [
        "5d43888a3c972fe68c224f93d41b30e9f888df7c", // 0: branch 1 near top
        "d7542c9db7f4c77dab4b315edd328edf1514952f", // 1: branch 1 near bottom
        "1d8a907f7b4bf50c6a09c16361e2205047ecc5e5", // 2: branch 2
        "15c40d0abc36d47fb51c8eaec51ac7aad31f669c", // 3: base
    ] {
        let changeset = repo
            .changeset(HgChangesetId::from_str(hg_hash)?)
            .await
            .expect("changeset exists");
        changesets.push(changeset);
    }
    for (index, base_index, is_ancestor_of) in [
        (0usize, 0usize, true),
        (0, 1, false),
        (0, 2, false),
        (0, 3, false),
        (1, 0, true),
        (1, 1, true),
        (1, 2, false),
        (1, 3, false),
        (2, 0, false),
        (2, 1, false),
        (2, 2, true),
        (2, 3, false),
        (3, 0, true),
        (3, 1, true),
        (3, 2, true),
        (3, 3, true),
    ] {
        assert_eq!(
            changesets[index]
                .as_ref()
                .unwrap()
                .is_ancestor_of(changesets[base_index].as_ref().unwrap().id())
                .await?,
            is_ancestor_of,
            "changesets[{index}].is_ancestor_of(changesets[{base_index}].id()) == {is_ancestor_of}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn commit_path_exists_and_type(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke = Mononoke::new_test(vec![(
        "test".to_string(),
        ManyFilesDirs::get_repo(fb).await,
    )])
    .await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let hash = "b0d1bf77898839595ee0f0cba673dd6e3be9dadaaa78bc6dd2dea97ca6bee77e";
    let cs_id = ChangesetId::from_str(hash)?;
    let cs = repo.changeset(cs_id).await?.expect("changeset exists");

    let root_path = cs.root().await?;
    assert!(root_path.exists().await?);
    assert!(root_path.is_tree().await?);

    let dir1_path = cs.path_with_content("dir1").await?;
    assert!(dir1_path.exists().await?);
    assert!(dir1_path.is_tree().await?);
    assert_eq!(dir1_path.file_type().await?, None);

    let file1_path = cs.path_with_content("dir1/file_1_in_dir1").await?;
    assert!(file1_path.exists().await?);
    assert!(!file1_path.is_tree().await?);
    assert_eq!(file1_path.file_type().await?, Some(FileType::Regular));

    let nonexistent_path = cs.path_with_content("nonexistent").await?;
    assert!(!nonexistent_path.exists().await?);
    assert!(!nonexistent_path.is_tree().await?);
    assert_eq!(nonexistent_path.file_type().await?, None);

    Ok(())
}

#[mononoke::fbinit_test]
async fn tree_list(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke = Mononoke::new_test(vec![(
        "test".to_string(),
        ManyFilesDirs::get_repo(fb).await,
    )])
    .await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let hash = "b0d1bf77898839595ee0f0cba673dd6e3be9dadaaa78bc6dd2dea97ca6bee77e";
    let cs_id = ChangesetId::from_str(hash)?;
    let cs = repo.changeset(cs_id).await?.expect("changeset exists");
    assert_eq!(
        {
            let path = cs.root().await?;
            let tree = path.tree().await?.unwrap();
            tree.list()
                .await?
                .into_iter()
                .map(|(name, _entry)| name)
                .collect::<Vec<_>>()
        },
        vec![
            String::from("1"),
            String::from("2"),
            String::from("dir1"),
            String::from("dir2")
        ]
    );
    assert_eq!(
        {
            let path = cs.path_with_content("dir1").await?;
            let tree = path.tree().await?.unwrap();
            tree.list()
                .await?
                .into_iter()
                .map(|(name, _entry)| name)
                .collect::<Vec<_>>()
        },
        vec![
            String::from("file_1_in_dir1"),
            String::from("file_2_in_dir1"),
            String::from("subdir1"),
        ]
    );
    let subsubdir2_id = {
        // List `dir1/subdir1`, but also capture a subtree id.
        let path = cs.path_with_content("dir1/subdir1").await?;
        let tree = path.tree().await?.unwrap();
        assert_eq!(
            {
                tree.list()
                    .await?
                    .into_iter()
                    .map(|(name, _entry)| name)
                    .collect::<Vec<_>>()
            },
            vec![
                String::from("file_1"),
                String::from("subsubdir1"),
                String::from("subsubdir2")
            ]
        );
        match tree
            .list()
            .await?
            .into_iter()
            .collect::<HashMap<_, _>>()
            .get("subsubdir2")
            .expect("entry should exist for subsubdir2")
        {
            ContentManifestEntry::Directory(dir) => dir.id,
            entry => panic!("subsubdir2 entry should be a directory, not {entry:?}"),
        }
    };
    assert_eq!(
        {
            let path = cs.path_with_content("dir1/subdir1/subsubdir1").await?;
            let tree = path.tree().await?.unwrap();
            tree.list()
                .await?
                .into_iter()
                .map(|(name, entry)| match entry {
                    ContentManifestEntry::File(file) => {
                        Some((name, file.size, file.content_id.to_string()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        },
        vec![Some((
            String::from("file_1"),
            9,
            String::from("50703cfe0783933daaa0fc4dcaec0dec16760f7dc77e553e666ae62cebae9cc6")
        ))]
    );
    // Get tree by id
    assert_eq!(
        {
            let tree = repo.tree(subsubdir2_id).await?.expect("tree exists");
            tree.list()
                .await?
                .into_iter()
                .map(|(name, _entry)| name)
                .collect::<Vec<_>>()
        },
        vec![String::from("file_1"), String::from("file_2")]
    );
    // Get tree by non-existent id returns None.
    assert!(
        repo.tree(mononoke_types::ContentManifestId::from_bytes([1; 32]).unwrap())
            .await?
            .is_none()
    );
    // Get tree by non-existent path returns None.
    {
        let path = cs.path_with_content("nonexistent").await?;
        assert!(path.tree().await?.is_none());
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn file_metadata(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke = Mononoke::new_test(vec![(
        "test".to_string(),
        ManyFilesDirs::get_repo(fb).await,
    )])
    .await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    let expected_metadata = FileMetadata {
        content_id: FileId::from_str(
            "9d9cf646b38852094ec48ab401eea6f4481cc89a80589331845dc08f75a652d2",
        )?,
        total_size: 9,
        sha1: Sha1::from_str("b29930dda02406077d96a7b7a08ce282b3de6961")?,
        sha256: Sha256::from_str(
            "47d741b6059c6d7e99be25ce46fb9ba099cfd6515de1ef7681f93479d25996a4",
        )?,
        git_sha1: RichGitSha1::from_sha1(
            GitSha1::from_str("ac3e272b72bbf89def8657766b855d0656630ed4")?,
            "blob",
            9,
        ),
        seeded_blake3: Blake3::from_str(
            "bbd41b606802e603ef6eae081bed1f29eb1ffb897f98e670bbd770c3dba8b81d",
        )?,
        is_binary: false,
        is_ascii: true,
        is_utf8: true,
        ends_in_newline: true,
        newline_count: 1,
        first_line: Some(String::from("content1")),
        is_generated: false,
        is_partially_generated: false,
    };

    // Get file by changeset path.
    let hash = "b0d1bf77898839595ee0f0cba673dd6e3be9dadaaa78bc6dd2dea97ca6bee77e";
    let cs_id = ChangesetId::from_str(hash)?;
    let cs = repo.changeset(cs_id).await?.expect("changeset exists");

    let path = cs.path_with_content("dir1/file_1_in_dir1").await?;
    let file = path.file().await?.unwrap();
    let metadata = file.metadata().await?;
    assert_eq!(metadata, expected_metadata);

    // Get file by content id.
    let file = repo
        .file(FileId::from_str(
            "9d9cf646b38852094ec48ab401eea6f4481cc89a80589331845dc08f75a652d2",
        )?)
        .await?
        .expect("file exists");
    let metadata = file.metadata().await?;
    assert_eq!(metadata, expected_metadata);

    // Get file by content sha1.
    let file = repo
        .file_by_content_sha1(Sha1::from_str("b29930dda02406077d96a7b7a08ce282b3de6961")?)
        .await?
        .expect("file exists");
    let metadata = file.metadata().await?;
    assert_eq!(metadata, expected_metadata);

    // Get file by content sha256.
    let file = repo
        .file_by_content_sha256(Sha256::from_str(
            "47d741b6059c6d7e99be25ce46fb9ba099cfd6515de1ef7681f93479d25996a4",
        )?)
        .await?
        .expect("file exists");
    let metadata = file.metadata().await?;
    assert_eq!(metadata, expected_metadata);

    // Get file by content seeded blake3
    let file = repo
        .file_by_content_seeded_blake3(Blake3::from_str(
            "bbd41b606802e603ef6eae081bed1f29eb1ffb897f98e670bbd770c3dba8b81d",
        )?)
        .await?
        .expect("file exists");
    let metadata = file.metadata().await?;
    assert_eq!(metadata, expected_metadata);

    Ok(())
}

#[mononoke::fbinit_test]
async fn file_metadata_of_redacted_content(fb: FacebookInit) -> Result<(), Error> {
    // What it tests: file metadata read through the API for a file whose content is redacted.
    // Expected: the first line is not returned, the rest of the metadata is unchanged, and
    // the metadata of an unredacted file still has its first line.
    let ctx = CoreContext::test_mock(fb);
    let mut factory = TestRepoFactory::new(fb)?;
    let repo: Repo = factory.build().await?;
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("redacted", "redacted first line\nsecond line\n")
        .add_file("visible", "visible first line\nsecond line\n")
        .commit()
        .await?;

    let file_metadata = |repo: Repo, path: &'static str| {
        let ctx = ctx.clone();
        async move {
            let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
            let repo = mononoke
                .repo(ctx, "test")
                .await?
                .expect("repo exists")
                .build()
                .await?;
            let cs = repo.changeset(cs_id).await?.expect("changeset exists");
            let file = cs
                .path_with_content(path)
                .await?
                .file()
                .await?
                .expect("file exists");
            anyhow::Ok(file.metadata().await?)
        }
    };

    let unredacted_metadata = file_metadata(repo, "redacted").await?;
    assert_eq!(
        unredacted_metadata.first_line.as_deref(),
        Some("redacted first line"),
        "the first line should be returned before the content is redacted"
    );

    let redacted_repo: Repo = factory
        .redacted(Some(RedactedBlobs::FromHashMapForTests(Arc::new(
            hashmap! {
                unredacted_metadata.content_id.blobstore_key() => RedactedMetadata {
                    task: "T12345".to_string(),
                    log_only: false,
                },
            },
        ))))
        .build()
        .await?;

    let redacted_metadata = file_metadata(redacted_repo.clone(), "redacted").await?;
    assert_eq!(
        redacted_metadata,
        FileMetadata {
            first_line: None,
            ..unredacted_metadata
        },
        "only the first line should be removed from the metadata of redacted content"
    );

    let visible_metadata = file_metadata(redacted_repo, "visible").await?;
    assert_eq!(
        visible_metadata.first_line.as_deref(),
        Some("visible first line"),
        "the metadata of unredacted content should keep its first line"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn file_contents(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke = Mononoke::new_test(vec![(
        "test".to_string(),
        ManyFilesDirs::get_repo(fb).await,
    )])
    .await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    let hash = "b0d1bf77898839595ee0f0cba673dd6e3be9dadaaa78bc6dd2dea97ca6bee77e";
    let cs_id = ChangesetId::from_str(hash)?;
    let cs = repo.changeset(cs_id).await?.expect("changeset exists");

    let path = cs.path_with_content("dir1/file_1_in_dir1").await?;
    let file = path.file().await?.unwrap();
    let content = file.content_concat().await?;
    assert_eq!(content, Bytes::from("content1\n"));

    let content_range = file.content_range_concat(3, 4).await?;
    assert_eq!(content_range, Bytes::from("tent"));

    Ok(())
}

#[mononoke::fbinit_test]
async fn xrepo_commit_lookup_simple(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (mononoke, _cfg_src) = init_x_repo(&ctx).await?;

    let smallrepo = mononoke
        .repo(ctx.clone(), "smallrepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let largerepo = mononoke
        .repo(ctx.clone(), "largerepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    let small_master_cs_id = resolve_cs_id(&ctx, smallrepo.repo(), "master").await?;

    info!("remapping {} from small to large", small_master_cs_id);
    // Confirm that a cross-repo lookup for an unsynced commit just fails
    let cs = smallrepo
        .xrepo_commit_lookup(
            &largerepo,
            small_master_cs_id,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await?
        .expect("changeset should exist");
    let large_master_cs_id = resolve_cs_id(&ctx, largerepo.repo(), "master").await?;
    assert_eq!(cs.id(), large_master_cs_id);

    info!("remapping {} from large to small", large_master_cs_id);
    let cs = largerepo
        .xrepo_commit_lookup(
            &smallrepo,
            large_master_cs_id,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await?
        .expect("changeset should exist");
    assert_eq!(cs.id(), small_master_cs_id);
    Ok(())
}

#[mononoke::fbinit_test]
async fn xrepo_commit_lookup_draft(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (mononoke, _cfg_src) = init_x_repo(&ctx).await?;

    let smallrepo = mononoke
        .repo(ctx.clone(), "smallrepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let small_master_cs_id = resolve_cs_id(&ctx, smallrepo.repo(), "master").await?;
    let largerepo = mononoke
        .repo(ctx.clone(), "largerepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let large_master_cs_id = resolve_cs_id(&ctx, largerepo.repo(), "master").await?;

    let new_large_draft =
        CreateCommitContext::new(&ctx, largerepo.repo(), vec![large_master_cs_id])
            .add_file("prefix/remapped", "content1")
            .add_file("not_remapped", "content2")
            .commit()
            .await?;

    let cs = largerepo
        .xrepo_commit_lookup(
            &smallrepo,
            new_large_draft,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await?;
    assert!(cs.is_some());
    let bcs = cs
        .unwrap()
        .id()
        .load(&ctx, smallrepo.repo().repo_blobstore())
        .await?;
    let file_changes: Vec<_> = bcs.file_changes().map(|(path, _)| path).cloned().collect();
    assert_eq!(file_changes, vec![NonRootMPath::new("remapped")?]);

    // Now in another direction
    let new_small_draft =
        CreateCommitContext::new(&ctx, smallrepo.repo(), vec![small_master_cs_id])
            .add_file("remapped2", "content2")
            .commit()
            .await?;
    let cs = smallrepo
        .xrepo_commit_lookup(
            &largerepo,
            new_small_draft,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await?;
    assert!(cs.is_some());
    let bcs = cs
        .unwrap()
        .id()
        .load(&ctx, largerepo.repo().repo_blobstore())
        .await?;
    let file_changes: Vec<_> = bcs.file_changes().map(|(path, _)| path).cloned().collect();
    assert_eq!(file_changes, vec![NonRootMPath::new("prefix/remapped2")?]);

    Ok(())
}

#[mononoke::fbinit_test]
async fn xrepo_commit_lookup_public(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (mononoke, _cfg_src) = init_x_repo(&ctx).await?;

    let smallrepo = mononoke
        .repo(ctx.clone(), "smallrepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let small_master_cs_id = resolve_cs_id(&ctx, smallrepo.repo(), "master").await?;
    let largerepo = mononoke
        .repo(ctx.clone(), "largerepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let large_master_cs_id = resolve_cs_id(&ctx, largerepo.repo(), "master").await?;

    let new_large_public =
        CreateCommitContext::new(&ctx, largerepo.repo(), vec![large_master_cs_id])
            .add_file("prefix/remapped", "content1")
            .add_file("not_remapped", "content2")
            .commit()
            .await?;

    bookmark(&ctx, largerepo.repo(), "publicbook")
        .set_to(new_large_public)
        .await?;

    let cs = largerepo
        .xrepo_commit_lookup(
            &smallrepo,
            new_large_public,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await?;
    assert!(cs.is_some());
    let bcs = cs
        .unwrap()
        .id()
        .load(&ctx, smallrepo.repo().repo_blobstore())
        .await?;
    let file_changes: Vec<_> = bcs.file_changes().map(|(path, _)| path).cloned().collect();
    assert_eq!(file_changes, vec![NonRootMPath::new("remapped")?]);

    // Now in another direction - it should fail
    let new_small_public =
        CreateCommitContext::new(&ctx, smallrepo.repo(), vec![small_master_cs_id])
            .add_file("remapped2", "content2")
            .commit()
            .await?;
    bookmark(&ctx, smallrepo.repo(), "newsmallpublicbook")
        .set_to(new_small_public)
        .await?;
    let res = smallrepo
        .xrepo_commit_lookup(
            &largerepo,
            new_small_public,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await;
    assert!(res.is_err());

    Ok(())
}

#[mononoke::fbinit_test]
async fn xrepo_commit_lookup_config_changing_live(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (mononoke, cfg_src) = init_x_repo(&ctx).await?;

    let smallrepo = mononoke
        .repo(ctx.clone(), "smallrepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let largerepo = mononoke
        .repo(ctx.clone(), "largerepo")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let small_master_cs_id = resolve_cs_id(&ctx, smallrepo.repo(), "master").await?;
    let large_master_cs_id = resolve_cs_id(&ctx, largerepo.repo(), "master").await?;

    // Before config change
    let first_large = CreateCommitContext::new(&ctx, largerepo.repo(), vec![large_master_cs_id])
        .add_file("prefix/remapped_before", "content1")
        .add_file("not_remapped", "content2")
        .commit()
        .await?;

    let first_small = largerepo
        .xrepo_commit_lookup(
            &smallrepo,
            first_large,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await?;
    let file_changes: Vec<_> = first_small
        .unwrap()
        .id()
        .load(&ctx, smallrepo.repo().repo_blobstore())
        .await?
        .file_changes()
        .map(|(path, _)| path)
        .cloned()
        .collect();

    assert_eq!(file_changes, vec![NonRootMPath::new("remapped_before")?]);

    // Config change: new config remaps prefix2 instead of prefix
    let large_repo_id = largerepo.repo().repo_identity().id();
    let small_repo_id = smallrepo.repo().repo_identity().id();
    let mut cfg = cfg_src
        .get_commit_sync_config_by_version_if_exists(
            large_repo_id,
            &CommitSyncConfigVersion("TEST_VERSION_NAME".to_string()),
        )?
        .unwrap();
    cfg.small_repos
        .get_mut(&small_repo_id)
        .unwrap()
        .default_action = DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(
        NonRootMPath::new("prefix2").unwrap(),
    );
    let new_version = CommitSyncConfigVersion("TEST_VERSION_NAME_2".to_string());
    cfg.version_name = new_version.clone();
    cfg_src.add_config(cfg.clone());

    let change_mapping_small =
        CreateCommitContext::new(&ctx, smallrepo.repo(), vec![small_master_cs_id])
            .commit()
            .await?;
    let change_mapping_large =
        CreateCommitContext::new(&ctx, largerepo.repo(), vec![large_master_cs_id])
            .commit()
            .await?;

    let commit_sync_repos = CommitSyncRepos::new(
        smallrepo.repo().clone(),
        largerepo.repo().clone(),
        CommitSyncDirection::Backwards,
        SubmoduleDeps::ForSync(HashMap::new()),
    );

    let commit_sync_data =
        CommitSyncData::new(&ctx, commit_sync_repos, largerepo.live_commit_sync_config());

    update_mapping_with_version(
        &ctx,
        hashmap! {change_mapping_large => change_mapping_small},
        &commit_sync_data,
        &new_version,
    )
    .await?;

    // After config change
    let second_large = CreateCommitContext::new(&ctx, largerepo.repo(), vec![change_mapping_large])
        .add_file("prefix2/remapped_after", "content1")
        .add_file("not_remapped", "content2")
        .commit()
        .await?;

    let second_small = largerepo
        .xrepo_commit_lookup(
            &smallrepo,
            second_large,
            None,
            XRepoLookupSyncBehaviour::SyncIfAbsent,
            XRepoLookupExactBehaviour::WorkingCopyEquivalence,
        )
        .await?;
    let file_changes: Vec<_> = second_small
        .unwrap()
        .id()
        .load(&ctx, smallrepo.repo().repo_blobstore())
        .await?
        .file_changes()
        .map(|(path, _)| path)
        .cloned()
        .collect();

    assert_eq!(file_changes, vec![NonRootMPath::new("remapped_after")?]);
    Ok(())
}

async fn init_x_repo(
    ctx: &CoreContext,
) -> Result<(Mononoke<Repo>, TestLiveCommitSyncConfigSource), Error> {
    let (syncers, commit_sync_config, _lv_cfg, lv_cfg_src) =
        init_small_large_repo::<crate::Repo>(ctx).await?;

    let small_to_large = syncers.small_to_large;
    let mononoke = Mononoke::new_test_xrepo(
        small_to_large.get_small_repo().clone(),
        small_to_large.get_large_repo().clone(),
    )
    .await?;
    lv_cfg_src.add_config(commit_sync_config);
    Ok((mononoke, lv_cfg_src))
}

#[mononoke::fbinit_test]
async fn resolve_changeset_id_prefix(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke =
        Mononoke::new_test(vec![("test".to_string(), Linear::get_repo(fb).await)]).await?;

    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    let hg_cs_id = ChangesetSpecifier::Hg(HgChangesetId::from_str(
        "607314ef579bd2407752361ba1b0c1729d08b281",
    )?);

    let bonsai_cs_id = ChangesetSpecifier::Bonsai(ChangesetId::from_str(
        "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6",
    )?);

    // test different lengths
    let test_cases: Vec<(_, Vec<ChangesetPrefixSpecifier>)> = vec![
        (
            &hg_cs_id,
            vec![
                HgChangesetIdPrefix::from_str("6073")?.into(),
                HgChangesetIdPrefix::from_str("607314e")?.into(),
                HgChangesetIdPrefix::from_str("607314ef57")?.into(),
                HgChangesetIdPrefix::from_str("607314ef579bd2407752361ba")?.into(),
                HgChangesetIdPrefix::from_str("607314ef579bd2407752361ba1b0c1729d08b281")?.into(),
            ],
        ),
        (
            &bonsai_cs_id,
            vec![
                ChangesetIdPrefix::from_str("7785")?.into(),
                ChangesetIdPrefix::from_str("7785606")?.into(),
                ChangesetIdPrefix::from_str("7785606eb1f26f")?.into(),
                ChangesetIdPrefix::from_str("7785606eb1f26ff5722c831")?.into(),
                ChangesetIdPrefix::from_str(
                    "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6",
                )?
                .into(),
            ],
        ),
    ];

    for (expected, prefixes) in test_cases {
        for prefix in prefixes {
            assert_eq!(
                repo.resolve_changeset_id_prefix(prefix).await?,
                ChangesetSpecifierPrefixResolution::Single(*expected)
            );
        }
    }

    // nonexistent changeset
    assert_eq!(
        ChangesetSpecifierPrefixResolution::NoMatch,
        repo.resolve_changeset_id_prefix(HgChangesetIdPrefix::from_str("607314efffff")?.into())
            .await?
    );

    // invalid hex string
    assert!(HgChangesetIdPrefix::from_str("607314euuuuu").is_err());

    Ok(())
}

struct LinearLoader(FacebookInit);

impl RepoLoader<Repo> for LinearLoader {
    fn load(&self, _repo_name: String) -> BoxFuture<'static, Result<Repo>> {
        let fb = self.0;
        async move { Ok(Linear::get_repo(fb).await) }.boxed()
    }
}

#[mononoke::fbinit_test]
async fn repo_build_wait(fb: FacebookInit) -> Result<(), Error> {
    let repos = Arc::new(MononokeRepos::new_lazy(Arc::new(LinearLoader(fb))));
    repos.add_lazy_repo("test", 0);
    let mononoke = Mononoke::new(repos, Arc::new(ArcSwap::from_pointee(HashMap::new())))?;
    let wait_us = |ctx: &CoreContext| {
        ctx.perf_counters()
            .get_counter(PerfCounterType::RepoBuildWaitUs)
    };

    let building = CoreContext::test_mock(fb);
    mononoke
        .repo(building.clone(), "test")
        .await?
        .expect("repo exists");
    assert!(
        wait_us(&building) > 0,
        "the request that built the repo counts its wait"
    );

    let after = CoreContext::test_mock(fb);
    mononoke
        .repo(after.clone(), "test")
        .await?
        .expect("repo exists");
    assert_eq!(
        wait_us(&after),
        0,
        "a request for a built repo waits on no build"
    );

    let unknown = CoreContext::test_mock(fb);
    assert!(mononoke.repo(unknown.clone(), "unknown").await?.is_none());
    assert_eq!(
        wait_us(&unknown),
        0,
        "a repo not served here builds nothing"
    );

    Ok(())
}
