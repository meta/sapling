/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![type_length_limit = "2000000"]

use anyhow::Error;
use anyhow::anyhow;
use blobstore::KeyedBlobstore;
use bytes::Bytes;
use bytes::BytesMut;
use cloned::cloned;
use context::CoreContext;
use futures::future::Future;
use futures::future::TryFutureExt;
use futures::stream;
use futures::stream::Stream;
use futures::stream::TryStreamExt;
use mononoke_types::BlobstoreKey;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::FileContents;
use mononoke_types::hash;

mod alias;
mod chunk;
mod copy;
mod errors;
mod expected_size;
mod fetch;
mod fetch_key;
mod finalize;
mod incremental_hash;
mod metadata;
mod multiplexer;
mod prepare;
mod rechunk;
mod streamhash;

pub use alias::add_aliases_to_multiplexer;
pub use chunk::Chunks;
pub use chunk::make_chunks;
pub use copy::copy;
pub use expected_size::ExpectedSize;
pub use fetch::Range;
pub use fetch::stream_file_bytes;
pub use fetch_key::AliasBlob;
pub use fetch_key::FetchKey;
pub use incremental_hash::Blake3IncrementalHasher;
pub use incremental_hash::GitSha1IncrementalHasher;
pub use incremental_hash::Sha1IncrementalHasher;
pub use incremental_hash::Sha256IncrementalHasher;
pub use incremental_hash::hash_bytes;
pub use mononoke_types::Alias;
pub use rechunk::force_rechunk;
pub use rechunk::rechunk;

#[cfg(test)]
mod test;

/// File storage.
///
/// This is a specialized wrapper around a blobstore specifically for user data files (
/// rather than metadata, trees, etc). Its primary (initial) goals are:
/// - providing a streaming interface for file access
/// - maintain multiple aliases for each file using different key schemes
/// - maintain reverse mapping from primary key to aliases
///
/// Secondary:
/// - Implement chunking at this level
/// - Compression
/// - Range access (initially fetch, and later store)
///
/// Implementation notes:
/// This code takes over the management of file content in a backwards compatible way - it uses
/// the same blobstore key structure and the same encoding schemes for existing files.
/// Extensions (compression, chunking) will change this, but it will still allow backwards
/// compatibility.
#[facet::facet]
#[derive(Debug, Copy, Clone)]
pub struct FilestoreConfig {
    pub chunk_size: Option<u64>,
    pub concurrency: usize,
}

impl FilestoreConfig {
    pub fn no_chunking_filestore() -> Self {
        Self {
            chunk_size: None,
            concurrency: 1,
        }
    }
}

/// Key for storing. We'll compute any missing keys, but we must have the total size.
#[derive(Debug, Clone)]
pub struct StoreRequest {
    pub expected_size: expected_size::ExpectedSize,
    pub canonical: Option<ContentId>,
    pub sha1: Option<hash::Sha1>,
    pub sha256: Option<hash::Sha256>,
    pub git_sha1: Option<hash::RichGitSha1>,
    pub seeded_blake3: Option<hash::Blake3>,
}

impl StoreRequest {
    pub fn new(size: u64) -> Self {
        Self {
            expected_size: ExpectedSize::new(size),
            canonical: None,
            sha1: None,
            sha256: None,
            git_sha1: None,
            seeded_blake3: None,
        }
    }

    pub fn with_canonical(size: u64, canonical: ContentId) -> Self {
        Self {
            expected_size: ExpectedSize::new(size),
            canonical: Some(canonical),
            sha1: None,
            sha256: None,
            git_sha1: None,
            seeded_blake3: None,
        }
    }

    pub fn with_sha1(size: u64, sha1: hash::Sha1) -> Self {
        Self {
            expected_size: ExpectedSize::new(size),
            canonical: None,
            sha1: Some(sha1),
            sha256: None,
            git_sha1: None,
            seeded_blake3: None,
        }
    }

    pub fn with_sha256(size: u64, sha256: hash::Sha256) -> Self {
        Self {
            expected_size: ExpectedSize::new(size),
            canonical: None,
            sha1: None,
            sha256: Some(sha256),
            git_sha1: None,
            seeded_blake3: None,
        }
    }

    pub fn with_git_sha1(size: u64, git_sha1: hash::RichGitSha1) -> Self {
        Self {
            expected_size: ExpectedSize::new(size),
            canonical: None,
            sha1: None,
            sha256: None,
            git_sha1: Some(git_sha1),
            seeded_blake3: None,
        }
    }

    pub fn with_seeded_blake3(size: u64, seeded_blake3: hash::Blake3) -> Self {
        Self {
            expected_size: expected_size::ExpectedSize::new(size),
            canonical: None,
            sha1: None,
            sha256: None,
            git_sha1: None,
            seeded_blake3: Some(seeded_blake3),
        }
    }

    pub fn with_fetch_key(size: u64, key: FetchKey) -> Self {
        use FetchKey::*;
        match key {
            Canonical(id) => Self::with_canonical(size, id),
            Aliased(Alias::Sha1(id)) => Self::with_sha1(size, id),
            Aliased(Alias::GitSha1(id)) => {
                Self::with_git_sha1(size, hash::RichGitSha1::from_sha1(id, "blob", size))
            }
            Aliased(Alias::Sha256(id)) => Self::with_sha256(size, id),
            Aliased(Alias::SeededBlake3(id)) => Self::with_seeded_blake3(size, id),
        }
    }
}

/// Fetch the metadata for the underlying content. This will return None if the content does
/// not exist. It might recompute metadata on the fly if the content exists but the metadata does
/// not.
pub async fn get_metadata<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    key: &FetchKey,
) -> Result<Option<ContentMetadataV2>, Error> {
    match key.content_id(ctx, blobstore).await? {
        Some(id) => metadata::get_metadata(blobstore, ctx, id).await,
        None => Ok(None),
    }
}

/// Fetch the metadata for the underlying content. This will return None if the content does
/// not exist, Some(None) if the metadata does not exist, and Some(Some(ContentMetadata))
/// when metadata found. It will not recompute metadata on the fly
pub async fn get_metadata_readonly<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    key: &FetchKey,
) -> Result<Option<Option<ContentMetadataV2>>, Error> {
    match key.content_id(ctx, blobstore).await? {
        Some(id) => metadata::get_metadata_readonly(blobstore, ctx, id)
            .await
            .map(Some),
        None => Ok(Some(None)),
    }
}

/// Compute the metadata of a content from its bytes, as `get_metadata` does when the stored
/// metadata is absent or in an older format, but store nothing: neither the metadata nor its
/// aliases. This will return None if the content does not exist.
pub async fn compute_metadata<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    content_id: ContentId,
) -> Result<Option<ContentMetadataV2>, Error> {
    match metadata::compute_metadata(blobstore, ctx, content_id).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(metadata::RebuildBackmappingError::NotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Return true if the given key exists. A successful return means the key definitely
/// either exists or doesn't; an error means the existence could not be determined.
pub async fn exists<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    key: &FetchKey,
) -> Result<bool, Error> {
    match key.content_id(ctx, blobstore).await? {
        Some(id) => blobstore
            .is_present(ctx, &id.blobstore_key())
            .await?
            .fail_if_unsure(),
        None => Ok(false),
    }
}

/// Fetch a file as a stream. This returns either success with a stream of data and file size if
/// the file exists, success with None if it does not exist, or an Error if either existence can't
/// be determined or if opening the file failed. File contents are returned in chunks configured by
/// FilestoreConfig::read_chunk_size - this defines the max chunk size, but they may be shorter
/// (not just the final chunks - any of them). Chunks are guaranteed to have non-zero size.
pub async fn fetch_with_size<'a, B: KeyedBlobstore + Clone + 'static>(
    blobstore: B,
    ctx: &CoreContext,
    key: &FetchKey,
) -> Result<Option<(impl Stream<Item = Result<Bytes, Error>> + use<'a, B>, u64)>, Error> {
    match key.content_id(ctx, &blobstore).await? {
        Some(content_id) => {
            fetch::fetch_with_size(blobstore, ctx, content_id, fetch::Range::all()).await
        }
        None => Ok(None),
    }
}

/// This function has the same functionality as fetch_with_size, but only
/// returns data for a range within the file.
///
/// Requests for data beyond the end of the file will return only the part of
/// the file that overlaps with the requested range, if any.
pub async fn fetch_range_with_size<'a, B: KeyedBlobstore + Clone + 'static>(
    blobstore: B,
    ctx: &CoreContext,
    key: &FetchKey,
    range: Range,
) -> Result<Option<(impl Stream<Item = Result<Bytes, Error>> + use<'a, B>, u64)>, Error> {
    match key.content_id(ctx, &blobstore).await? {
        Some(content_id) => fetch::fetch_with_size(blobstore, ctx, content_id, range).await,
        None => Ok(None),
    }
}

/// This function has the same functionality as fetch_with_size, but doesn't return the file size.
pub async fn fetch<'a, B: KeyedBlobstore + Clone + 'static>(
    blobstore: B,
    ctx: &CoreContext,
    key: &FetchKey,
) -> Result<Option<impl Stream<Item = Result<Bytes, Error>> + use<'a, B>>, Error> {
    let res = fetch_with_size(blobstore, ctx, key).await?;
    Ok(res.map(|(stream, _len)| stream))
}

/// Fetch the contents of a blob concatenated together.
async fn fetch_concat_impl<B: KeyedBlobstore + Clone + 'static>(
    blobstore: &B,
    ctx: &CoreContext,
    key: &FetchKey,
    size: Option<u64>,
) -> Result<Option<Bytes>, Error> {
    let res = fetch_with_size(blobstore.clone(), ctx, key).await?;

    match res {
        Some((stream, len)) => {
            if let Some(size) = size {
                if len != size {
                    return Err(anyhow!(
                        "Fetched content length ({len}) is not the expected size ({size})"
                    ));
                }
            }
            let len = len
                .try_into()
                .map_err(|_| anyhow!("Cannot fetch file with length {len}"))?;

            let buf = BytesMut::with_capacity(len);

            let bytes = stream
                .try_fold(buf, |mut buffer, chunk| async move {
                    buffer.extend_from_slice(&chunk);
                    Result::<_, Error>::Ok(buffer)
                })
                .await?;

            Ok(Some(bytes.freeze()))
        }
        None => Ok(None),
    }
}

/// Fetch the contents of a blob concatenated together. This bad for buffering, and you shouldn't
/// add new callsites. This is only for compatibility with existing callsites.
pub async fn fetch_concat_opt<B: KeyedBlobstore + Clone + 'static>(
    blobstore: &B,
    ctx: &CoreContext,
    key: &FetchKey,
) -> Result<Option<Bytes>, Error> {
    fetch_concat_impl(blobstore, ctx, key, None).await
}

/// Similar to `fetch_concat_opt`, but requires the blob to be present, or errors out.
pub async fn fetch_concat<B: KeyedBlobstore + Clone + 'static>(
    blobstore: &B,
    ctx: &CoreContext,
    key: impl Into<FetchKey>,
) -> Result<Bytes, Error> {
    let key: FetchKey = key.into();
    let bytes = fetch_concat_impl(blobstore, ctx, &key, None).await?;
    bytes.ok_or_else(|| errors::FilestoreError::MissingContent(key).into())
}

/// Similar to `fetch_concat, but requires the content to be a known exact
/// size.  Suitable for use when the size is known and relatively small.
pub async fn fetch_concat_exact<'a, B: KeyedBlobstore + Clone + 'static>(
    blobstore: &'a B,
    ctx: &'a CoreContext,
    key: impl Into<FetchKey>,
    size: u64,
) -> Result<Bytes, Error> {
    let key = key.into();
    fetch_concat_impl(blobstore, ctx, &key, Some(size))
        .await?
        .ok_or_else(|| errors::FilestoreError::MissingContent(key).into())
}

/// Fetch content associated with the key as a stream
///
/// Mostly behaves as the `fetch`, except it is pushing missing content error into stream if
/// data associated with the key was not found.
pub fn fetch_stream<'a, B: KeyedBlobstore + Clone + 'static, K: Into<FetchKey>>(
    blobstore: B,
    ctx: &CoreContext,
    key: K,
) -> impl Stream<Item = Result<Bytes, Error>> + use<'a, B, K> {
    let ctx = ctx.clone();
    let key: FetchKey = key.into();

    async move {
        let stream = fetch(blobstore, &ctx, &key)
            .await?
            .ok_or(errors::FilestoreError::MissingContent(key))?;
        Result::<_, Error>::Ok(stream)
    }
    .try_flatten_stream()
}

/// This function has the same functionality as fetch_range_with_size, but doesn't return the file size.
pub async fn fetch_range<'a, B: KeyedBlobstore + Clone + 'static>(
    blobstore: &'a B,
    ctx: &'a CoreContext,
    key: &FetchKey,
    range: Range,
) -> Result<Option<impl Stream<Item = Result<Bytes, Error>> + use<'a, B>>, Error> {
    let res = fetch_range_with_size(blobstore.clone(), ctx, key, range).await?;
    Ok(res.map(|(stream, _len)| stream))
}

/// Fetch the start of a file. Returns a Future that resolves with Some(Bytes) if the file was
/// found, and None otherwise. If Bytes are found, this function is guaranteed to return as many
/// Bytes as requested unless the file is shorter than that.
pub async fn peek<B: KeyedBlobstore + Clone + 'static>(
    blobstore: &B,
    ctx: &CoreContext,
    key: &FetchKey,
    size: usize,
) -> Result<Option<Bytes>, Error> {
    let maybe_stream = fetch(blobstore.clone(), ctx, key).await?;

    match maybe_stream {
        None => Ok(None),
        Some(stream) => {
            let mut stream = chunk::ChunkStream::new(stream, size);
            stream.try_next().await
        }
    }
}

/// Store a file from a stream. This is guaranteed atomic - either the store will succeed
/// for the entire file, or it will fail and the file will logically not exist (however
/// there's no guarantee that any partially written parts will be cleaned up).
pub async fn store<B: KeyedBlobstore + Clone + 'static>(
    blobstore: &B,
    config: FilestoreConfig,
    ctx: &CoreContext,
    req: &StoreRequest,
    data: impl Stream<Item = Result<Bytes, Error>> + Send,
) -> Result<ContentMetadataV2, Error> {
    let prepared = match chunk::make_chunks(data, req.expected_size, config.chunk_size) {
        Chunks::Inline(fut) => prepare::prepare_bytes(fut.await?).await,
        Chunks::Chunked(expected_size, chunks) => {
            prepare::prepare_chunked(
                ctx.clone(),
                blobstore.clone(),
                expected_size,
                chunks,
                config.concurrency,
            )
            .await?
        }
    };

    finalize::finalize(blobstore, ctx, Some(req), prepared).await
}

/// Store a set of bytes, and immediately return their Contentid and size. This function is
/// inefficient for large files, since it will hash the file twice if it's larger than the chunk
/// size. This function is intended as a transition function while we convert writers to streams
/// and refactor them to not expect to be able to obtain the ContentId for the content they
/// uploaded immediately. Do NOT add new callsites.
pub fn store_bytes<B: KeyedBlobstore + Clone + 'static>(
    blobstore: &B,
    config: FilestoreConfig,
    ctx: &CoreContext,
    bytes: Bytes,
) -> (
    (ContentId, u64),
    impl Future<Output = Result<(), Error>> + Send + use<B>,
) {
    // NOTE: Like in other places in the Filestore, we assume that the size of buffers being passed
    // in can be represented in 64 bits (which is OK for the world we live in).

    let content_id = FileContents::content_id_for_bytes(&bytes);
    let size: u64 = bytes.len().try_into().unwrap();

    cloned!(ctx, blobstore);
    let upload = async move {
        store(
            &blobstore,
            config,
            &ctx,
            &StoreRequest::with_canonical(size, content_id),
            stream::once(async move { Ok(bytes) }),
        )
        .await?;

        Ok(())
    };

    ((content_id, size), upload)
}
