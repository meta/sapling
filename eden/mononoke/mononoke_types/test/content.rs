/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use blobstore::Loadable;
use bytes::Bytes;
use context::CoreContext;
use fbinit::FacebookInit;
use memblob::Memblob;
use mononoke_macros::mononoke;
use mononoke_types::Alias;
use mononoke_types::BlobstoreKey;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::ContentMetadataV2Id;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::Notice;
use mononoke_types::audit::ObjectId;
use mononoke_types::audit::StoredObject;
use mononoke_types::audit::ValidationOutcome;
use mononoke_types::audit::WalkError;
use mononoke_types::hash::Sha256;
use mononoke_types::thrift;
use repo_blobstore::RepoBlobstoreRef;
use tests_utils::CreateCommitContext;
use tests_utils::audit::load_ctx;
use tests_utils::audit::walk;

use crate::test_utils::TestRepo;
use crate::test_utils::get_raw;
use crate::test_utils::put_raw;
use crate::test_utils::remove_late_fields;
use crate::test_utils::repo_key;
use crate::test_utils::rewrite_metadata;
use crate::test_utils::store_chunked;
use crate::test_utils::test_repo;
use crate::test_utils::test_repo_counting_reads;

/// A root commit adding one file, its content and that content's four
/// aliases, as the filestore stored them when the file was uploaded.
async fn one_file(
    ctx: &CoreContext,
    repo: &TestRepo,
) -> Result<(ChangesetId, ContentId, [Alias; 4])> {
    one_file_with(ctx, repo, "one").await
}

/// As `one_file`, with the file holding `content`.
async fn one_file_with(
    ctx: &CoreContext,
    repo: &TestRepo,
    content: &str,
) -> Result<(ChangesetId, ContentId, [Alias; 4])> {
    let commit = CreateCommitContext::new_root(ctx, repo)
        .add_file("a", content)
        .commit()
        .await?;
    let changeset = Loadable::load(&commit, ctx, repo.repo_blobstore()).await?;
    let content_id = changeset
        .simplified_file_changes()
        .find_map(|(_path, change)| Some(change?.content_id()))
        .context("root commit adds one file")?;
    let metadata = filestore::get_metadata_readonly(repo.repo_blobstore(), ctx, &content_id.into())
        .await?
        .flatten()
        .context("the filestore stores metadata on upload")?;
    let aliases = [
        Alias::Sha1(metadata.sha1),
        Alias::Sha256(metadata.sha256),
        Alias::GitSha1(metadata.git_sha1.sha1()),
        Alias::SeededBlake3(metadata.seeded_blake3),
    ];
    Ok((commit, content_id, aliases))
}

#[mononoke::fbinit_test]
async fn core_emits_metadata_and_four_aliases(fb: FacebookInit) -> Result<()> {
    // Given a commit adding one file
    let ctx = CoreContext::test_mock(fb);
    let (repo, _blobstore, reads) = test_repo_counting_reads(fb).await?;
    let (commit, content_id, aliases) = one_file(&ctx, &repo).await?;

    // When the core family walks it
    reads.store(0, Ordering::SeqCst);
    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4).await??;

    // Then it emits the changeset, the content, its metadata and its four
    // alias mappings, each once
    let ids = objects
        .iter()
        .map(|object| object.id().clone())
        .collect::<Vec<_>>();
    let expected = [
        ObjectId::of::<TestRepo, _>(&commit),
        ObjectId::of::<TestRepo, _>(&content_id),
        ObjectId::of::<TestRepo, _>(&ContentMetadataV2Id::from(content_id)),
    ]
    .into_iter()
    .chain(aliases.iter().map(ObjectId::of::<TestRepo, Alias>))
    .collect::<Vec<_>>();
    assert_eq!(ids.len(), expected.len(), "{ids:?}");
    assert_eq!(
        ids.iter().collect::<HashSet<_>>(),
        expected.iter().collect::<HashSet<_>>()
    );
    assert_eq!(
        ids.iter().map(|id| id.name()).collect::<Vec<_>>(),
        [
            "Changeset",
            "FileContent",
            "FileContentMetadataV2",
            "AliasContentMapping",
            "AliasContentMapping",
            "AliasContentMapping",
            "AliasContentMapping",
        ],
        "the metadata and aliases follow their content"
    );

    // And every one of them validates, each read once: the changeset, the
    // content, its metadata and its four alias mappings
    for object in objects {
        assert_eq!(
            object
                .validate(&load_ctx(&ctx, &*repo), &*repo, true)
                .await?,
            vec![]
        );
    }
    assert_eq!(reads.load(Ordering::SeqCst), 7);
    Ok(())
}

#[mononoke::fbinit_test]
async fn an_alias_mapping_to_another_content_is_unreadable(fb: FacebookInit) -> Result<()> {
    for index in 0..4 {
        // Given a commit whose content's alias mapping was replaced by the
        // same kind of alias mapping of another content
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (commit, _content_id, aliases) = one_file(&ctx, &repo).await?;
        let (_other_commit, _other_content_id, other_aliases) =
            one_file_with(&ctx, &repo, "two").await?;
        let replaced = aliases[index];
        let other = get_raw(
            &ctx,
            &repo,
            &blobstore,
            other_aliases[index].blobstore_key(),
        )
        .await?
        .context("the other content's alias mapping is stored")?;
        put_raw(&ctx, &repo, &blobstore, replaced.blobstore_key(), other).await?;

        // When the core family walks it and the alias is validated
        let alias = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4)
            .await??
            .into_iter()
            .find(|object| *object.id() == ObjectId::of::<TestRepo, _>(&replaced))
            .context("core emits the alias")?;
        let result = alias.validate(&load_ctx(&ctx, &*repo), &*repo, false).await;

        // Then it is unreadable, since it does not point back to the content
        assert_eq!(
            result.map_err(|error| error.class()),
            Err("unreadable"),
            "{replaced:?}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn current_metadata_naming_another_content_is_unreadable(fb: FacebookInit) -> Result<()> {
    // Given a commit whose content's metadata, in the current format, names
    // another content
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    let (_other_commit, other_content_id, _other_aliases) =
        one_file_with(&ctx, &repo, "two").await?;
    rewrite_metadata(&ctx, &repo, &blobstore, content_id, |record| {
        record.content_id = Some(other_content_id.into_thrift());
    })
    .await?;

    // When the core family walks it, then the metadata is unreadable at
    // load, naming the content it holds
    let cause = assert_metadata_unreadable(&ctx, &repo, commit, content_id).await?;
    assert!(cause.contains(&other_content_id.to_string()), "{cause}");
    Ok(())
}

/// Removes the stored metadata of `content_id`, if `metadata` is false, and
/// the mapping of `removed`, then walks `commit` and validates `removed`.
async fn validate_removed_alias(
    ctx: &CoreContext,
    repo: &Arc<TestRepo>,
    blobstore: &Memblob,
    (commit, content_id): (ChangesetId, ContentId),
    metadata: bool,
    removed: Alias,
) -> Result<Result<Vec<ValidationOutcome>, LoadError>> {
    if !metadata {
        let key = ContentMetadataV2Id::from(content_id).blobstore_key();
        assert_eq!(blobstore.unlink(repo_key(repo, key)).await?, Some(()));
    }
    assert_eq!(
        blobstore
            .unlink(repo_key(repo, removed.blobstore_key()))
            .await?,
        Some(())
    );
    let alias = walk::<BonsaiChangeset, _>(ctx, repo, commit, 4)
        .await??
        .into_iter()
        .find(|object| *object.id() == ObjectId::of::<TestRepo, _>(&removed))
        .context("core emits the alias even though its mapping is absent")?;
    Ok(alias
        .validate(&load_ctx(ctx, &**repo), &**repo, false)
        .await)
}

#[mononoke::fbinit_test]
async fn absent_alias_mapping_fails_validation_as_missing(fb: FacebookInit) -> Result<()> {
    for index in [1, 3] {
        // Given a commit whose content has current metadata but whose SHA-256
        // or seeded Blake3 alias mapping is removed
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (commit, content_id, aliases) = one_file(&ctx, &repo).await?;
        let removed = aliases[index];

        // When the core family walks it and the removed alias is validated
        let result =
            validate_removed_alias(&ctx, &repo, &blobstore, (commit, content_id), true, removed)
                .await?;

        // Then it is missing
        assert_eq!(
            result.map_err(|error| error.class()),
            Err("missing"),
            "{removed:?}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn absent_metadata_is_recomputed_and_noticed(fb: FacebookInit) -> Result<()> {
    // Given a commit whose content's metadata was never stored
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, aliases) = one_file(&ctx, &repo).await?;
    let metadata_id = ContentMetadataV2Id::from(content_id);
    assert_eq!(
        blobstore
            .unlink(repo_key(&repo, metadata_id.blobstore_key()))
            .await?,
        Some(())
    );

    // When the core family walks it
    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4).await??;

    // Then it emits the metadata, noticed as absent, and the recomputed
    // metadata's aliases other than seeded Blake3
    assert_eq!(
        objects
            .iter()
            .map(|object| object.id().clone())
            .collect::<HashSet<_>>(),
        [
            ObjectId::of::<TestRepo, _>(&commit),
            ObjectId::of::<TestRepo, _>(&content_id),
            ObjectId::of::<TestRepo, _>(&metadata_id),
        ]
        .into_iter()
        .chain(aliases[..3].iter().map(ObjectId::of::<TestRepo, Alias>))
        .collect::<HashSet<_>>()
    );
    assert_eq!(
        notices(&objects),
        [
            ("Changeset", None),
            ("FileContent", None),
            ("FileContentMetadataV2", Some(Notice::AbsentContentMetadata)),
            ("AliasContentMapping", None),
            ("AliasContentMapping", None),
            ("AliasContentMapping", None),
        ]
    );

    // And every one of them validates
    for object in objects {
        assert_eq!(
            object
                .validate(&load_ctx(&ctx, &*repo), &*repo, true)
                .await?,
            vec![]
        );
    }

    // And nothing was stored: the metadata is still absent
    assert_eq!(
        get_raw(&ctx, &repo, &blobstore, metadata_id.blobstore_key()).await?,
        None
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn absent_metadata_and_seeded_blake3_alias_pass(fb: FacebookInit) -> Result<()> {
    // Given a commit whose content has neither metadata nor a seeded Blake3
    // alias mapping, as for contents the 2023 metadata backfill skipped
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, aliases) = one_file(&ctx, &repo).await?;
    let removed = aliases[3];
    assert!(matches!(removed, Alias::SeededBlake3(_)));
    for key in [
        ContentMetadataV2Id::from(content_id).blobstore_key(),
        removed.blobstore_key(),
    ] {
        assert_eq!(blobstore.unlink(repo_key(&repo, key)).await?, Some(()));
    }

    // When the core family walks it and validates what it emits
    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4).await??;

    // Then the seeded Blake3 alias is not checked, and everything else
    // validates
    assert!(
        objects
            .iter()
            .all(|object| *object.id() != ObjectId::of::<TestRepo, _>(&removed)),
        "{objects:?}"
    );
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
async fn absent_metadata_and_sha1_alias_fail_the_alias_as_missing(fb: FacebookInit) -> Result<()> {
    // Given a commit whose content has neither metadata nor a SHA-1 alias
    // mapping
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, aliases) = one_file(&ctx, &repo).await?;
    let removed = aliases[0];
    assert!(matches!(removed, Alias::Sha1(_)));

    // When the core family walks it and the removed alias is validated
    let result = validate_removed_alias(
        &ctx,
        &repo,
        &blobstore,
        (commit, content_id),
        false,
        removed,
    )
    .await?;

    // Then it is missing
    assert_eq!(result.map_err(|error| error.class()), Err("missing"));
    Ok(())
}

#[mononoke::fbinit_test]
async fn absent_content_and_metadata_report_the_content(fb: FacebookInit) -> Result<()> {
    // Given a commit whose content and its metadata are both removed, as
    // a lost upload leaves them
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    for removed in [
        content_id.blobstore_key(),
        ContentMetadataV2Id::from(content_id).blobstore_key(),
    ] {
        assert_eq!(blobstore.unlink(repo_key(&repo, removed)).await?, Some(()));
    }

    // When the core family walks it
    let error = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4)
        .await?
        .expect_err("the content and its metadata were removed");

    // Then, as in Walker, the content is reported missing, not its metadata
    match error {
        WalkError::Load {
            object,
            source: LoadError::Missing(_),
        } => assert_eq!(object, ObjectId::of::<TestRepo, _>(&content_id)),
        other => panic!("expected missing content at load, got {other:#}"),
    }
    Ok(())
}

/// Each object's type name and notice.
fn notices(objects: &[StoredObject<TestRepo>]) -> Vec<(&'static str, Option<Notice>)> {
    objects
        .iter()
        .map(|object| (object.id().name(), object.notice()))
        .collect()
}

#[mononoke::fbinit_test]
async fn old_format_metadata_is_recomputed_and_noticed(fb: FacebookInit) -> Result<()> {
    // Given a commit whose content's metadata was stored before spring
    // 2023, without the fields added then
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, aliases) = one_file(&ctx, &repo).await?;
    let metadata_id = ContentMetadataV2Id::from(content_id);
    let stored = rewrite_metadata(&ctx, &repo, &blobstore, content_id, remove_late_fields).await?;
    assert!(
        ContentMetadataV2::from_bytes(&stored).is_err(),
        "the old record does not decode"
    );

    // When the core family walks it
    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4).await??;

    // Then it emits the metadata, noticed, and the recomputed metadata's
    // aliases other than seeded Blake3, which the old record lacks
    assert_eq!(
        objects
            .iter()
            .map(|object| object.id().clone())
            .collect::<HashSet<_>>(),
        [
            ObjectId::of::<TestRepo, _>(&commit),
            ObjectId::of::<TestRepo, _>(&content_id),
            ObjectId::of::<TestRepo, _>(&metadata_id),
        ]
        .into_iter()
        .chain(aliases[..3].iter().map(ObjectId::of::<TestRepo, Alias>))
        .collect::<HashSet<_>>()
    );
    assert_eq!(
        notices(&objects),
        [
            ("Changeset", None),
            ("FileContent", None),
            (
                "FileContentMetadataV2",
                Some(Notice::OldFormatContentMetadata)
            ),
            ("AliasContentMapping", None),
            ("AliasContentMapping", None),
            ("AliasContentMapping", None),
        ]
    );

    // And every one of them validates
    for object in objects {
        assert_eq!(
            object
                .validate(&load_ctx(&ctx, &*repo), &*repo, true)
                .await?,
            vec![]
        );
    }

    // And nothing was stored: the old record is still there
    assert_eq!(
        get_raw(&ctx, &repo, &blobstore, metadata_id.blobstore_key()).await?,
        Some(stored)
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn metadata_lacking_only_seeded_blake3_is_recomputed(fb: FacebookInit) -> Result<()> {
    // Given metadata that lacks only one of the fields added in spring 2023
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    rewrite_metadata(&ctx, &repo, &blobstore, content_id, |record| {
        record.seeded_blake3 = None
    })
    .await?;

    // When the core family walks it
    let objects = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4).await??;

    // Then it is recomputed and noticed too
    assert!(
        notices(&objects).contains(&(
            "FileContentMetadataV2",
            Some(Notice::OldFormatContentMetadata)
        )),
        "{objects:?}"
    );
    Ok(())
}

/// Walks `commit`, expecting its content's metadata to fail at load as
/// unreadable.
async fn assert_metadata_unreadable(
    ctx: &CoreContext,
    repo: &Arc<TestRepo>,
    commit: ChangesetId,
    content_id: ContentId,
) -> Result<String> {
    let error = walk::<BonsaiChangeset, _>(ctx, repo, commit, 4)
        .await?
        .expect_err("the metadata is damaged");
    match error {
        WalkError::Load {
            object,
            source: source @ LoadError::Unreadable(_),
        } => {
            assert_eq!(
                object,
                ObjectId::of::<TestRepo, _>(&ContentMetadataV2Id::from(content_id))
            );
            Ok(format!("{source:#}"))
        }
        other => panic!("expected unreadable metadata at load, got {other:#}"),
    }
}

#[mononoke::fbinit_test]
async fn old_format_metadata_disagreeing_with_its_content_is_unreadable(
    fb: FacebookInit,
) -> Result<()> {
    // Given an old record whose SHA-256 is not its content's
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    rewrite_metadata(&ctx, &repo, &blobstore, content_id, |record| {
        remove_late_fields(record);
        record.sha256 = Some(Sha256::from_byte_array([7; 32]).into_thrift());
    })
    .await?;

    // When the core family walks it, then the metadata is unreadable at
    // load, naming the hash that differs
    let cause = assert_metadata_unreadable(&ctx, &repo, commit, content_id).await?;
    assert!(cause.contains("sha256"), "{cause}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn old_format_metadata_lacking_another_field_is_unreadable(fb: FacebookInit) -> Result<()> {
    // Given an old record that also lacks its SHA-1
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    rewrite_metadata(&ctx, &repo, &blobstore, content_id, |record| {
        remove_late_fields(record);
        record.sha1 = None;
    })
    .await?;

    // When the core family walks it, then the metadata is unreadable
    assert_metadata_unreadable(&ctx, &repo, commit, content_id).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn current_metadata_with_a_wrong_length_hash_is_unreadable(fb: FacebookInit) -> Result<()> {
    // Given a record with every current field whose SHA-256 is one byte
    // short
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    rewrite_metadata(&ctx, &repo, &blobstore, content_id, |record| {
        record.sha256 = Some(thrift::id::Sha256(vec![7; 31].into()));
    })
    .await?;

    // When the core family walks it, then the metadata is unreadable rather
    // than recomputed
    assert_metadata_unreadable(&ctx, &repo, commit, content_id).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn undecodable_metadata_is_unreadable(fb: FacebookInit) -> Result<()> {
    // Given metadata whose bytes are not a record at all
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    put_raw(
        &ctx,
        &repo,
        &blobstore,
        ContentMetadataV2Id::from(content_id).blobstore_key(),
        Bytes::from_static(b"garbage"),
    )
    .await?;

    // When the core family walks it, then the metadata is unreadable
    assert_metadata_unreadable(&ctx, &repo, commit, content_id).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn old_format_metadata_of_absent_content_reports_the_content(fb: FacebookInit) -> Result<()> {
    // Given an old record whose content is lost, so it cannot be recomputed
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (commit, content_id, _aliases) = one_file(&ctx, &repo).await?;
    rewrite_metadata(&ctx, &repo, &blobstore, content_id, remove_late_fields).await?;
    assert_eq!(
        blobstore
            .unlink(repo_key(&repo, content_id.blobstore_key()))
            .await?,
        Some(())
    );

    // When the core family walks it
    let error = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4)
        .await?
        .expect_err("the content was removed");

    // Then the content is reported missing
    match error {
        WalkError::Load {
            object,
            source: LoadError::Missing(_),
        } => assert_eq!(object, ObjectId::of::<TestRepo, _>(&content_id)),
        other => panic!("expected missing content at load, got {other:#}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn old_format_metadata_of_content_missing_a_chunk_reports_the_content(
    fb: FacebookInit,
) -> Result<()> {
    // Given a commit adding a file stored in two-byte chunks, whose metadata
    // is in the old format and one of whose chunks is lost
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (content_id, chunks) = store_chunked(&ctx, &repo, b"chunked").await?;
    let commit = CreateCommitContext::new_root(&ctx, &*repo)
        .add_file("a", "chunked")
        .commit()
        .await?;
    rewrite_metadata(&ctx, &repo, &blobstore, content_id, remove_late_fields).await?;
    assert_eq!(
        blobstore
            .unlink(repo_key(&repo, chunks[1].blobstore_key()))
            .await?,
        Some(())
    );

    // When the core family walks it
    let error = walk::<BonsaiChangeset, _>(&ctx, &repo, commit, 4)
        .await?
        .expect_err("a chunk was removed");

    // Then the metadata cannot be recomputed, and the content is reported
    // missing
    match error {
        WalkError::Load {
            object,
            source: LoadError::Missing(_),
        } => assert_eq!(object, ObjectId::of::<TestRepo, _>(&content_id)),
        other => panic!("expected missing content at load, got {other:#}"),
    }
    Ok(())
}
