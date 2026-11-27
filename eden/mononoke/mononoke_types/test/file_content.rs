/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::atomic::Ordering;

use anyhow::Result;
use blobstore::Blobstore;
use context::CoreContext;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ContentChunk;
use mononoke_types::audit::ObjectId;
use mononoke_types::audit::StoredObject;
use mononoke_types::audit::Validatable;
use repo_identity::RepoIdentityRef;
use tests_utils::audit::load_ctx;

use crate::test_utils::TestRepo;
use crate::test_utils::store_chunked;
use crate::test_utils::test_repo;
use crate::test_utils::test_repo_counting_reads;

#[mononoke::fbinit_test]
async fn chunked_content_reads_every_chunk(fb: FacebookInit) -> Result<()> {
    // Given a six-byte file stored in three chunks
    let ctx = CoreContext::test_mock(fb);
    let (repo, _blobstore, reads) = test_repo_counting_reads(fb).await?;
    let (content_id, chunks) = store_chunked(&ctx, &repo, b"abcdef").await?;
    assert_eq!(chunks.len(), 3);

    // When it is loaded
    let load_ctx = load_ctx(&ctx, &*repo);
    let tagged = load_ctx.tag(ObjectId::of::<TestRepo, _>(&content_id));
    reads.store(0, Ordering::SeqCst);
    let size = Validatable::<TestRepo>::load(&content_id, &tagged, &*repo).await?;

    // Then every chunk's bytes were read, with one read for the file and one
    // for each chunk
    assert_eq!(size, 6);
    assert_eq!(reads.load(Ordering::SeqCst), 4);
    Ok(())
}

#[mononoke::fbinit_test]
async fn a_chunk_of_another_size_than_its_file_records_is_unreadable(
    fb: FacebookInit,
) -> Result<()> {
    for (bytes, case) in [
        (&b"c"[..], "one byte short"),
        (&b"cde"[..], "one byte long"),
    ] {
        // Given a file stored in two-byte chunks whose middle chunk holds
        // a byte fewer or more than the file's chunk list records
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (content_id, chunks) = store_chunked(&ctx, &repo, b"abcdef").await?;
        assert_eq!(chunks.len(), 3);
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            chunks[1].blobstore_key()
        );
        assert_eq!(blobstore.unlink(key.clone()).await?, Some(()));
        blobstore
            .put(&ctx, key, ContentChunk::new_bytes(bytes).into_blob().into())
            .await?;

        // When it is validated
        let error = StoredObject::<TestRepo>::new(content_id)
            .validate(&load_ctx(&ctx, &*repo), &*repo, false)
            .await
            .expect_err("a chunk has the wrong size");

        // Then it is unreadable
        assert_eq!(error.class(), "unreadable", "{case}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn each_missing_chunk_fails_validation_as_missing(fb: FacebookInit) -> Result<()> {
    // Given a file stored in three chunks
    let ctx = CoreContext::test_mock(fb);
    let (repo, blobstore) = test_repo(fb).await?;
    let (content_id, chunks) = store_chunked(&ctx, &repo, b"abcdef").await?;
    assert_eq!(chunks.len(), 3);

    for chunk in chunks {
        // When it is validated with that chunk removed
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            chunk.blobstore_key()
        );
        let bytes = blobstore
            .get(&ctx, &key)
            .await?
            .expect("the chunk is stored")
            .into_bytes();
        assert_eq!(blobstore.unlink(key.clone()).await?, Some(()));
        let error = StoredObject::<TestRepo>::new(content_id)
            .validate(&load_ctx(&ctx, &*repo), &*repo, false)
            .await
            .expect_err("a chunk was removed");

        // Then the absent chunk is missing, not unreadable
        assert_eq!(error.class(), "missing", "{chunk} removed");
        blobstore.put(&ctx, key, bytes).await?;
    }
    Ok(())
}
