/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::Loadable;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use bytes::Bytes;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use fbinit::FacebookInit;
use fbthrift::compact_protocol;
use filestore::FilestoreConfig;
use filestore::StoreRequest;
use futures::stream;
use memblob::Memblob;
use metaconfig_types::RepoConfig;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ContentChunkId;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::ContentMetadataV2Id;
use mononoke_types::FileContents;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::RecomputeContentMetadata;
use mononoke_types::audit::TaggedContext;
use mononoke_types::thrift;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use test_repo_factory::TestRepoFactory;
use tests_utils::BasicTestRepo;

/// The shared test repo, as its own type so that it can recompute content
/// metadata.
#[facet::container]
pub(crate) struct TestRepo {
    #[delegate(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        CommitGraph,
        dyn CommitGraphWriter,
        FilestoreConfig,
        RepoBlobstore,
        RepoConfig,
        RepoDerivedData,
        RepoIdentity
    )]
    basic: BasicTestRepo,
}

#[async_trait]
impl RecomputeContentMetadata for TestRepo {
    async fn recompute_content_metadata(
        &self,
        ctx: &TaggedContext,
        content: ContentId,
    ) -> Result<ContentMetadataV2, LoadError> {
        filestore::compute_metadata(ctx.blobstore(), ctx, content)
            .await
            .map_err(LoadError::Unreadable)?
            .ok_or_else(|| LoadError::Missing(content.blobstore_key()))
    }
}

pub(crate) async fn test_repo(fb: FacebookInit) -> Result<(Arc<TestRepo>, Memblob)> {
    let (repo, blobstore, _reads) = test_repo_counting_reads(fb).await?;
    Ok((repo, blobstore))
}

/// A test repo, its blobstore, and the number of reads its blobstore has
/// served.
pub(crate) async fn test_repo_counting_reads(
    fb: FacebookInit,
) -> Result<(Arc<TestRepo>, Memblob, Arc<AtomicUsize>)> {
    let blobstore = Memblob::default();
    let reads = Arc::new(AtomicUsize::new(0));
    let repo: TestRepo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(ReadCountingBlobstore {
            inner: blobstore.clone(),
            reads: reads.clone(),
        }))
        .build()
        .await?;
    Ok((Arc::new(repo), blobstore, reads))
}

/// Counts the reads that reach `inner`.
#[derive(Debug)]
struct ReadCountingBlobstore {
    inner: Memblob,
    reads: Arc<AtomicUsize>,
}

impl fmt::Display for ReadCountingBlobstore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ReadCountingBlobstore<{}>", self.inner)
    }
}

#[async_trait]
impl Blobstore for ReadCountingBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        self.inner
            .put_explicit(ctx, key, value, put_behaviour)
            .await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.inner.put_with_status(ctx, key, value).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        Blobstore::unlink(&self.inner, ctx, key).await
    }
}

/// Stores `content`'s metadata again with `edit` applied to its Thrift form,
/// replacing the record the filestore stored, and returns the bytes stored.
pub(crate) async fn rewrite_metadata(
    ctx: &CoreContext,
    repo: &TestRepo,
    blobstore: &Memblob,
    content: ContentId,
    edit: impl FnOnce(&mut thrift::content::ContentMetadataV2),
) -> Result<Bytes> {
    let metadata_id = ContentMetadataV2Id::from(content);
    let metadata = Loadable::load(&metadata_id, ctx, repo.repo_blobstore()).await?;
    let mut record: thrift::content::ContentMetadataV2 =
        compact_protocol::deserialize(metadata.into_blob().data())?;
    edit(&mut record);
    let bytes = compact_protocol::serialize(&record);
    put_raw(
        ctx,
        repo,
        blobstore,
        metadata_id.blobstore_key(),
        bytes.clone(),
    )
    .await?;
    Ok(bytes)
}

/// Removes the metadata fields added in spring 2023, as metadata stored
/// before then lacks them.
pub(crate) fn remove_late_fields(record: &mut thrift::content::ContentMetadataV2) {
    record.is_generated = None;
    record.is_partially_generated = None;
    record.seeded_blake3 = None;
}

/// Replaces the bytes stored under the repo's `key`.
pub(crate) async fn put_raw(
    ctx: &CoreContext,
    repo: &TestRepo,
    blobstore: &Memblob,
    key: String,
    bytes: Bytes,
) -> Result<()> {
    blobstore
        .put_explicit(
            ctx,
            repo_key(repo, key),
            BlobstoreBytes::from_bytes(bytes),
            PutBehaviour::Overwrite,
        )
        .await?;
    Ok(())
}

/// The bytes stored under the repo's `key`.
pub(crate) async fn get_raw(
    ctx: &CoreContext,
    repo: &TestRepo,
    blobstore: &Memblob,
    key: String,
) -> Result<Option<Bytes>> {
    Ok(blobstore
        .get(ctx, &repo_key(repo, key))
        .await?
        .map(|data| data.into_raw_bytes()))
}

/// Stores `content` in two-byte chunks and returns its id and its chunks.
pub(crate) async fn store_chunked(
    ctx: &CoreContext,
    repo: &TestRepo,
    content: &'static [u8],
) -> Result<(ContentId, Vec<ContentChunkId>)> {
    let config = FilestoreConfig {
        chunk_size: Some(2),
        concurrency: 1,
    };
    let metadata = filestore::store(
        repo.repo_blobstore(),
        config,
        ctx,
        &StoreRequest::new(content.len() as u64),
        stream::once(async move { Ok(Bytes::from_static(content)) }),
    )
    .await?;
    let content_id = metadata.content_id;
    match Loadable::load(&content_id, ctx, repo.repo_blobstore()).await? {
        FileContents::Chunked(chunked) => Ok((
            content_id,
            chunked
                .into_chunks()
                .iter()
                .map(|chunk| chunk.chunk_id())
                .collect(),
        )),
        FileContents::Bytes(_) => bail!("{content_id} was stored unchunked"),
    }
}

/// The repo's storage key for `key`, as its blobstore stores it.
pub(crate) fn repo_key(repo: &TestRepo, key: String) -> String {
    format!("{}{}", repo.repo_identity().id().prefix(), key)
}
