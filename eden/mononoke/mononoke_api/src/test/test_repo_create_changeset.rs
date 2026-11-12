/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::Error;
use assert_matches::assert_matches;
use bulk_derivation::BulkDerivation;
use bytes::Bytes;
use chrono::FixedOffset;
use chrono::TimeZone;
use ephemeral_blobstore::RepoEphemeralStoreRef;
use fbinit::FacebookInit;
use fixtures::Linear;
use fixtures::ManyFilesDirs;
use fixtures::TestRepoFixture;
use futures::try_join;
use mononoke_macros::mononoke;
use mononoke_types::DerivableType;
use mononoke_types::hash::Sha256;
use mononoke_types::path::MPath;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use smallvec::SmallVec;
use test_repo_factory::TestRepoFactory;

use crate::ChangesetContext;
use crate::ChangesetId;
use crate::CoreContext;
use crate::CreateChange;
use crate::CreateChangeFile;
use crate::CreateChangesetChecks;
use crate::CreateInfo;
use crate::FileType;
use crate::Mononoke;
use crate::MononokeError;
use crate::MononokeRepo;
use crate::RepoContext;
use crate::SNAPSHOT_BUBBLE_ID_EXTRA;
use crate::StoreRequest;
use crate::repo::Repo;
use crate::repo::create_changeset::CreateChangeFileContents;

#[mononoke::fbinit_test]
async fn test_create_commit(fb: FacebookInit) -> Result<(), Error> {
    create_commit(
        fb,
        &[
            DerivableType::SkeletonManifestsV2,
            DerivableType::ContentManifests,
        ],
    )
    .await?;

    Ok(())
}

// Check that commits were created correctly, and also check that only a single
// derived data type was derived (i.e. check that we don't derive something that we aren't supposed
// to).
async fn create_commit(
    fb: FacebookInit,
    derived_data_to_derive: &[DerivableType],
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, commits, _) = Linear::get_repo_and_dag(fb).await;
    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo = mononoke
        .repo(ctx.clone(), "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;
    let expected_hash = "68c9120f387cf1c3b7e4c2e30cdbd5b953f27a732cfe9f42f335f0091ece3c6c";
    let parents = vec![commits["K"]];
    let author = String::from("Test Author <test@example.com>");
    let author_date = FixedOffset::east_opt(0)
        .unwrap()
        .with_ymd_and_hms(2000, 2, 1, 12, 0, 0)
        .unwrap();
    let committer = None;
    let committer_date = None;
    let message = String::from("Test Created Commit");
    let extra = BTreeMap::new();
    let bubble = None;
    let mut changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    changes.insert(
        MPath::try_from("TEST_CREATE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE\n"), None),
    );

    // Pre-upload the file content for the second commit, and check its hash
    // on the way.
    let file_id = repo
        .upload_file_content(
            Bytes::from("TEST CREATE2\n"),
            &StoreRequest::with_sha256(
                13,
                Sha256::from_str(
                    "877f6bb6e0aeebc78c9b784ed633ef87019110bd61f867f0a4bf747085fec645",
                )?,
            ),
        )
        .await?;

    let cs = repo
        .create_changeset(
            parents,
            CreateInfo {
                author: author.clone(),
                author_date,
                committer: committer.clone(),
                committer_date,
                message: message.clone(),
                extra: extra.clone(),
                git_extra_headers: None,
            },
            changes.clone(),
            bubble,
            CreateChangesetChecks::check(),
        )
        .await?
        .changeset_ctx;

    changes.insert(
        MPath::try_from("TEST_CREATE")?,
        CreateChange::Tracked(
            CreateChangeFile {
                contents: CreateChangeFileContents::Existing {
                    file_id,
                    maybe_size: None,
                },
                file_type: Some(FileType::Regular),
                git_lfs: None,
            },
            None,
        ),
    );
    let second_cs = repo
        .create_changeset(
            vec![cs.id()],
            CreateInfo {
                author,
                author_date,
                committer,
                committer_date,
                message,
                extra,
                git_extra_headers: None,
            },
            changes,
            bubble,
            CreateChangesetChecks::check(),
        )
        .await?
        .changeset_ctx;

    validate_unnecessary_derived_data_is_not_derived(
        &ctx,
        &repo,
        cs.id(),
        second_cs.id(),
        derived_data_to_derive,
    )
    .await?;

    assert_eq!(cs.message().await?, "Test Created Commit");
    assert_eq!(cs.id(), ChangesetId::from_str(expected_hash)?);

    let content = cs
        .path_with_content("TEST_CREATE")
        .await?
        .file()
        .await?
        .expect("file should exist")
        .content_concat()
        .await?;
    assert_eq!(content, Bytes::from("TEST CREATE\n"));

    let content = second_cs
        .path_with_content("TEST_CREATE")
        .await?
        .file()
        .await?
        .expect("file should exist")
        .content_concat()
        .await?;
    assert_eq!(content, Bytes::from("TEST CREATE2\n"));

    Ok(())
}

// We expect that after creating a commit only derived a single specific derived data
// type is derived for a parent changeset, and none derived for the newly created changeset.
// This function validates it's actually the case
async fn validate_unnecessary_derived_data_is_not_derived<R: MononokeRepo>(
    ctx: &CoreContext,
    repo: &RepoContext<R>,
    parent_cs_id: ChangesetId,
    cs_id: ChangesetId,
    derived_data_to_derive: &[DerivableType],
) -> Result<(), Error> {
    for ty in &repo.repo().repo_derived_data_arc().active_config().types {
        let not_derived = repo
            .repo()
            .repo_derived_data()
            .manager()
            .pending(ctx, &[parent_cs_id, cs_id], None, *ty)
            .await?;
        // It's expected to derive skeleton manifests for the parent commit
        if derived_data_to_derive.contains(ty) {
            assert_eq!(not_derived, vec![cs_id]);
        } else {
            assert_eq!(not_derived, vec![parent_cs_id, cs_id]);
        }
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn create_commit_bad_changes(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, commits, _) = ManyFilesDirs::get_repo_and_dag(fb).await;
    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo = mononoke
        .repo(ctx, "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    async fn create_changeset<R: MononokeRepo>(
        repo: &RepoContext<R>,
        commits: &BTreeMap<String, ChangesetId>,
        changes: BTreeMap<MPath, CreateChange>,
    ) -> Result<ChangesetContext<R>, MononokeError> {
        let parents = vec![commits["C"]];
        let author = String::from("Test Author <test@example.com>");
        let author_date = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2000, 2, 1, 12, 0, 0)
            .unwrap();
        let committer = None;
        let committer_date = None;
        let message = String::from("Test Created Commit");
        let extra = BTreeMap::new();
        let bubble = None;
        let git_extra_headers =
            Some(maplit::btreemap! {SmallVec::new() => Bytes::from_static(b"world")});
        repo.create_changeset(
            parents,
            CreateInfo {
                author,
                author_date,
                committer,
                committer_date,
                message,
                extra,
                git_extra_headers,
            },
            changes,
            bubble,
            CreateChangesetChecks::check(),
        )
        .await
        .map(|created_changeset| created_changeset.changeset_ctx)
    }

    // Cannot delete a file that is not there
    let mut changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    changes.insert(MPath::try_from("TEST_CREATE")?, CreateChange::Deletion);
    assert_matches!(
        create_changeset(&repo, &commits, changes).await,
        Err(MononokeError::InvalidRequest(_))
    );

    // Cannot replace a file with a directory without deleting the file
    let mut changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    changes.insert(
        MPath::try_from("1/TEST_CREATE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("test"), None),
    );
    assert_matches!(
        create_changeset(&repo, &commits, changes.clone()).await,
        Err(MononokeError::InvalidRequest(_))
    );

    // Deleting the file means we can now replace it with a directory.
    changes.insert(MPath::try_from("1")?, CreateChange::Deletion);
    assert!(create_changeset(&repo, &commits, changes).await.is_ok());

    // Changes cannot introduce path conflicts
    let mut changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    changes.insert(
        MPath::try_from("TEST_CREATE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("test"), None),
    );
    changes.insert(
        MPath::try_from("TEST_CREATE/TEST_CREATE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("test"), None),
    );
    assert_matches!(
        create_changeset(&repo, &commits, changes).await,
        Err(MononokeError::InvalidRequest(_))
    );

    // Superfluous changes when a directory is replaced by a file are dropped
    let mut changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    changes.insert(
        MPath::try_from("dir1")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("test"), None),
    );
    let cs1 = create_changeset(&repo, &commits, changes.clone()).await?;

    changes.insert(
        MPath::try_from("dir1/file_1_in_dir1")?,
        CreateChange::Deletion,
    );
    changes.insert(
        MPath::try_from("dir1/subdir1/file_1")?,
        CreateChange::Deletion,
    );
    let cs2 = create_changeset(&repo, &commits, changes).await?;

    // Since the superfluous changes were dropped, the two commits
    // have the same bonsai hash.
    assert_eq!(cs1.id(), cs2.id());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_create_merge_commit(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, commits, _) = Linear::get_repo_and_dag(fb).await;
    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo = mononoke
        .repo(ctx.clone(), "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    async fn create_changeset<R: MononokeRepo>(
        repo: &RepoContext<R>,
        changes: BTreeMap<MPath, CreateChange>,
        parents: Vec<ChangesetId>,
    ) -> Result<ChangesetContext<R>, MononokeError> {
        let author = String::from("Test Author <test@example.com>");
        let author_date = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2000, 2, 1, 12, 0, 0)
            .unwrap();
        let committer = None;
        let committer_date = None;
        let message = String::from("Test Created Commit");
        let extra = BTreeMap::new();
        let bubble = None;
        let git_extra_headers = None;
        repo.create_changeset(
            parents,
            CreateInfo {
                author: author.clone(),
                author_date,
                committer: committer.clone(),
                committer_date,
                message: message.clone(),
                extra: extra.clone(),
                git_extra_headers,
            },
            changes.clone(),
            bubble,
            CreateChangesetChecks::check(),
        )
        .await
        .map(|created_changeset| created_changeset.changeset_ctx)
    }

    let initial_parents = vec![commits["K"]];
    let mut p1_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p1_changes.insert(
        MPath::try_from("TEST_CREATE_p1")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE\n"), None),
    );
    let mut p2_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p2_changes.insert(
        MPath::try_from("TEST_CREATE_p2")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE\n"), None),
    );

    let (p1, p2) = try_join!(
        create_changeset(&repo, p1_changes, initial_parents.clone(),),
        create_changeset(&repo, p2_changes, initial_parents)
    )?;

    let mut merge_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    merge_changes.insert(
        MPath::try_from("TEST_MERGE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE\n"), None),
    );
    create_changeset(&repo, merge_changes, vec![p1.id(), p2.id()]).await?;

    let mut merge_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    merge_changes.insert(
        MPath::try_from("TEST_CREATE_p1")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST MERGE OVERRIDE\n"), None),
    );
    create_changeset(&repo, merge_changes, vec![p1.id(), p2.id()]).await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_merge_commit_parent_file_conflict(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mononoke =
        Mononoke::new_test(vec![("test".to_string(), Linear::get_repo(fb).await)]).await?;
    let repo = mononoke
        .repo(ctx.clone(), "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    async fn create_changeset<R: MononokeRepo>(
        repo: &RepoContext<R>,
        changes: BTreeMap<MPath, CreateChange>,
        parents: Vec<ChangesetId>,
    ) -> Result<ChangesetContext<R>, MononokeError> {
        let author = String::from("Test Author <test@example.com>");
        let author_date = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2000, 2, 1, 12, 0, 0)
            .unwrap();
        let committer = None;
        let committer_date = None;
        let message = String::from("Test Created Commit");
        let extra = BTreeMap::new();
        let bubble = None;
        let git_extra_headers =
            Some(maplit::btreemap! {SmallVec::new() => Bytes::from_static(b"world")});
        repo.create_changeset(
            parents,
            CreateInfo {
                author: author.clone(),
                author_date,
                committer: committer.clone(),
                committer_date,
                message: message.clone(),
                extra: extra.clone(),
                git_extra_headers,
            },
            changes.clone(),
            bubble,
            CreateChangesetChecks::check(),
        )
        .await
        .map(|created_changeset| created_changeset.changeset_ctx)
    }

    let initial_hash = "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6";
    let initial_parents = vec![ChangesetId::from_str(initial_hash)?];
    let mut p1_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p1_changes.insert(
        MPath::try_from("TEST_FILE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE_p1\n"), None),
    );
    let mut p2_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p2_changes.insert(
        MPath::try_from("TEST_CREATE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE_p2\n"), None),
    );

    let mut p3_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p3_changes.insert(
        MPath::try_from("TEST_FILE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE_p3\n"), None),
    );

    let (p1, p2, p3) = try_join!(
        create_changeset(&repo, p1_changes, initial_parents.clone()),
        create_changeset(&repo, p2_changes, initial_parents.clone()),
        create_changeset(&repo, p3_changes, initial_parents.clone())
    )?;

    // p1 and p2 do not conflict
    create_changeset(&repo, BTreeMap::new(), vec![p1.id(), p2.id()]).await?;

    // p1 and p3 do conflict
    assert!(
        create_changeset(&repo, BTreeMap::new(), vec![p1.id(), p3.id()])
            .await
            .is_err()
    );

    // Can't hide it by making p3 third parent, or by moving p1 to second parent and p3 as third
    assert!(
        create_changeset(&repo, BTreeMap::new(), vec![p1.id(), p2.id(), p3.id()])
            .await
            .is_err()
    );
    assert!(
        create_changeset(&repo, BTreeMap::new(), vec![p2.id(), p1.id(), p3.id()])
            .await
            .is_err()
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_merge_commit_parent_tree_file_conflict(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, commits, _) = Linear::get_repo_and_dag(fb).await;
    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo = mononoke
        .repo(ctx.clone(), "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    async fn create_changeset<R: MononokeRepo>(
        repo: &RepoContext<R>,
        changes: BTreeMap<MPath, CreateChange>,
        parents: Vec<ChangesetId>,
    ) -> Result<ChangesetContext<R>, MononokeError> {
        let author = String::from("Test Author <test@example.com>");
        let author_date = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2000, 2, 1, 12, 0, 0)
            .unwrap();
        let committer = None;
        let committer_date = None;
        let message = String::from("Test Created Commit");
        let extra = BTreeMap::new();
        let bubble = None;
        let git_extra_headers =
            Some(maplit::btreemap! {SmallVec::new() => Bytes::from_static(b"world")});
        repo.create_changeset(
            parents,
            CreateInfo {
                author: author.clone(),
                author_date,
                committer: committer.clone(),
                committer_date,
                message: message.clone(),
                extra: extra.clone(),
                git_extra_headers,
            },
            changes.clone(),
            bubble,
            CreateChangesetChecks::check(),
        )
        .await
        .map(|created_changeset| created_changeset.changeset_ctx)
    }

    let initial_parents = vec![commits["K"]];
    let mut p1_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p1_changes.insert(
        MPath::try_from("TEST_FILE/REALLY_A_DIR")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE_p1\n"), None),
    );
    let mut p2_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p2_changes.insert(
        MPath::try_from("TEST_CREATE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE_p2\n"), None),
    );

    let mut p3_changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    p3_changes.insert(
        MPath::try_from("TEST_FILE")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("TEST CREATE_p3\n"), None),
    );

    let (p1, p2, p3) = try_join!(
        create_changeset(&repo, p1_changes, initial_parents.clone()),
        create_changeset(&repo, p2_changes, initial_parents.clone()),
        create_changeset(&repo, p3_changes, initial_parents.clone())
    )?;

    // p1 and p2 do not conflict
    create_changeset(&repo, BTreeMap::new(), vec![p1.id(), p2.id()]).await?;

    // p1 and p3 do conflict
    assert!(
        create_changeset(&repo, BTreeMap::new(), vec![p1.id(), p3.id()])
            .await
            .is_err()
    );

    // Can't hide it by making p3 third parent, or by moving p1 to second parent and p3 as third
    assert!(
        create_changeset(&repo, BTreeMap::new(), vec![p1.id(), p2.id(), p3.id()])
            .await
            .is_err()
    );
    assert!(
        create_changeset(&repo, BTreeMap::new(), vec![p2.id(), p1.id(), p3.id()])
            .await
            .is_err()
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_create_same_snapshot_in_two_bubbles(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = TestRepoFactory::new(fb)?
        .with_name("test".to_string())
        .with_ephemeral_store()
        .build()
        .await?;
    let (commits, _) = Linear::init_repo(fb, &repo).await?;
    let first_bubble = repo
        .repo_ephemeral_store()
        .create_bubble(&ctx, None, vec![])
        .await?;
    let second_bubble = repo
        .repo_ephemeral_store()
        .create_bubble(&ctx, None, vec![])
        .await?;
    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo = mononoke
        .repo(ctx.clone(), "test")
        .await?
        .expect("repo exists")
        .build()
        .await?;

    let info = || CreateInfo {
        author: String::from("Test Author <test@example.com>"),
        author_date: FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2000, 2, 1, 12, 0, 0)
            .unwrap(),
        committer: None,
        committer_date: None,
        message: String::new(),
        extra: BTreeMap::new(),
        git_extra_headers: None,
    };
    let mut changes: BTreeMap<MPath, CreateChange> = BTreeMap::new();
    changes.insert(
        MPath::try_from("SNAPSHOT_FILE")?,
        CreateChange::Untracked(CreateChangeFile::new_regular("SNAPSHOT\n")),
    );

    let first = repo
        .create_changeset(
            vec![commits["K"]],
            info(),
            changes.clone(),
            Some(&first_bubble),
            CreateChangesetChecks::check(),
        )
        .await?;
    let second = repo
        .create_changeset(
            vec![commits["K"]],
            info(),
            changes.clone(),
            Some(&second_bubble),
            CreateChangesetChecks::check(),
        )
        .await?;
    let first_again = repo
        .create_changeset(
            vec![commits["K"]],
            info(),
            changes,
            Some(&first_bubble),
            CreateChangesetChecks::check(),
        )
        .await?;

    assert_ne!(
        first.changeset_ctx.id(),
        second.changeset_ctx.id(),
        "the same snapshot in two bubbles should get two IDs"
    );
    assert_eq!(
        first.changeset_ctx.id(),
        first_again.changeset_ctx.id(),
        "the same snapshot in the same bubble should keep one ID"
    );
    assert_eq!(
        first.hg_extras.get(SNAPSHOT_BUBBLE_ID_EXTRA),
        Some(&first_bubble.bubble_id().to_string().into_bytes()),
        "the snapshot should record the bubble it was created in"
    );
    assert_eq!(
        repo.repo()
            .repo_ephemeral_store()
            .bubble_from_changeset(&ctx, &second.changeset_ctx.id())
            .await?,
        Some(second_bubble.bubble_id()),
        "the second snapshot should map only to the second bubble"
    );
    Ok(())
}
