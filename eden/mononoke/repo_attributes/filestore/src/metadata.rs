/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Error;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use blobstore::Storable;
use bytes::Bytes;
use context::CoreContext;
use futures::TryFutureExt;
use futures::future;
use futures::task::Poll;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ContentAlias;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::ContentMetadataV2Id;
use mononoke_types::errors::MononokeTypeError;
use strum::IntoEnumIterator;
use thiserror::Error;
use tracing::warn;

use crate::Alias;
use crate::AliasBlob;
use crate::alias::add_aliases_to_multiplexer;
use crate::expected_size::ExpectedSize;
use crate::fetch;
use crate::multiplexer::Multiplexer;
use crate::multiplexer::MultiplexerError;
use crate::prepare::add_partial_metadata_to_multiplexer;

#[derive(Debug, Error)]
pub enum RebuildBackmappingError {
    #[error("Not found: {0:?}")]
    NotFound(ContentId),

    #[error("Error computing metadata for {0:?}: {1:?}")]
    InternalError(ContentId, #[source] Error),
}

/// Finds the metadata for a ContentId. Returns None if the content does not exist, and returns
/// the metadata otherwise. This might recompute the metadata on the fly if it is found to
/// be missing but the content exists.
pub async fn get_metadata<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    content_id: ContentId,
) -> Result<Option<ContentMetadataV2>, Error> {
    let metadata = get_metadata_readonly(blobstore, ctx, content_id).await;
    // We found the metadata, return it.
    if let Ok(Some(_)) = metadata {
        return metadata;
    } else if let Err(e) = metadata {
        match e.downcast_ref::<MononokeTypeError>() {
            // The backfilling for ContentMetadataV2 has happened in different stages.
            // If any of the later fields are missing, we get invalid thrift error. In
            // that case we need to rebuild the metadata, so do not return.
            Some(MononokeTypeError::InvalidThrift(..)) => {
                let key = ContentMetadataV2Id::from(content_id).blobstore_key();
                let msg = format!(
                    "Invalid ContentMetadataV2 format exists in blobstore for key {key}. Error: {e}"
                );
                warn!("{}", &msg);
                let mut scuba = ctx.scuba().clone();
                scuba.add("blobstore_key", key);
                scuba.log_with_msg("ContentMetadataV2 backfill repair", msg);
            }
            _ => return Err(e),
        }
    }

    // We didn't find the metadata. Try to recompute it. This might fail if the
    // content doesn't exist, or due to an internal error.
    rebuild_metadata(blobstore, ctx, content_id)
        .await
        .map(Some)
        .or_else({
            use RebuildBackmappingError::*;
            |e| match e {
                // If we didn't find the ContentId we're rebuilding the metadata for,
                // then there is nothing else to do but indicate this metadata does not
                // exist.
                NotFound(_) => Ok(None),
                // If we ran into some error rebuilding the metadata that isn't not
                // having found the content, then we pass it up.
                e @ InternalError(..) => Err(e.into()),
            }
        })
}

/// Finds the metadata for a ContentId. Returns None if the content metadata does not exist
/// and returns Some(metadata) if it already exists. Does not recompute it on the fly.
pub async fn get_metadata_readonly<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    content_id: ContentId,
) -> Result<Option<ContentMetadataV2>, Error> {
    ContentMetadataV2Id::from(content_id)
        .load(ctx, blobstore)
        .await
        .map(Some)
        .or_else(|err| match err {
            LoadableError::Error(err) => Err(err),
            LoadableError::Missing(_) => Ok(None),
        })
}

/// If the metadata is missing, we can rebuild it on the fly, since all that's needed to do so
/// is the file contents. This can happen if we successfully stored a file, but failed to store
/// its metadata. To rebuild the metadata, we compute it from the contents, then store its
/// aliases and the metadata, and return it.
async fn rebuild_metadata<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    content_id: ContentId,
) -> Result<ContentMetadataV2, RebuildBackmappingError> {
    use RebuildBackmappingError::*;

    let metadata = compute_metadata(blobstore, ctx, content_id).await?;
    // To maintain consistency, rebuild the aliases to the content along with rebuilding
    // content metadata.
    let alias = ContentAlias::from_content_id(content_id);
    // Ensure that all aliases are covered, and missing out an alias gives a compile time error.
    future::try_join_all(
        Alias::iter()
            .map(|alias_type| match alias_type {
                Alias::Sha1(_) => AliasBlob(Alias::Sha1(metadata.sha1), alias.clone()),
                Alias::GitSha1(_) => {
                    AliasBlob(Alias::GitSha1(metadata.git_sha1.sha1()), alias.clone())
                }

                Alias::Sha256(_) => AliasBlob(Alias::Sha256(metadata.sha256), alias.clone()),
                Alias::SeededBlake3(_) => {
                    AliasBlob(Alias::SeededBlake3(metadata.seeded_blake3), alias.clone())
                }
            })
            .map(|alias_blob| alias_blob.store(ctx, blobstore)),
    )
    .await
    .map_err(|e| InternalError(content_id, e))?;

    let blob = metadata.clone().into_blob();

    blob.store(ctx, blobstore)
        .await
        .map_err(|e| InternalError(content_id, e))?;

    Ok(metadata)
}

/// Computes the metadata of a content from its bytes, storing nothing: we peek at the content
/// in the blobstore to get its size, then produce a stream of its contents and compute aliases
/// and the rest of the metadata over it.
pub(crate) async fn compute_metadata<B: KeyedBlobstore>(
    blobstore: &B,
    ctx: &CoreContext,
    content_id: ContentId,
) -> Result<ContentMetadataV2, RebuildBackmappingError> {
    use RebuildBackmappingError::*;

    let file_contents = content_id
        .load(ctx, blobstore)
        .await
        .map_err(|err| match err {
            LoadableError::Error(err) => InternalError(content_id, err),
            LoadableError::Missing(_) => NotFound(content_id),
        })?;

    // NOTE: We implicitly trust data from the Filestore here. We do not validate
    // the size, nor the ContentId.
    let total_size = file_contents.size();
    let content_stream =
        fetch::stream_file_bytes(blobstore, ctx, file_contents, fetch::Range::all())
            .map_err(|e| InternalError(content_id, e))?;

    let mut multiplexer = Multiplexer::<Bytes>::new();
    let aliases = add_aliases_to_multiplexer(&mut multiplexer, ExpectedSize::new(total_size))
        .map_err(|e| InternalError(content_id, e));
    let metadata = add_partial_metadata_to_multiplexer(&mut multiplexer)
        .map_err(|e| InternalError(content_id, e));

    let futs = future::try_join(aliases, metadata);
    let res = multiplexer.drain(content_stream).await;
    match res {
        // All is well - get the results when our futures complete.
        Ok(_) => {
            let (redeemable, metadata) = futs.await?;
            let (sha1, sha256, git_sha1, seeded_blake3) = redeemable
                .redeem(total_size)
                .map_err(|e| InternalError(content_id, e))?;
            Ok(ContentMetadataV2 {
                total_size,
                content_id,
                sha1,
                sha256,
                git_sha1,
                seeded_blake3,
                is_binary: metadata.is_binary,
                is_ascii: metadata.is_ascii,
                is_utf8: metadata.is_utf8,
                ends_in_newline: metadata.ends_in_newline,
                newline_count: metadata.newline_count,
                first_line: metadata.first_line,
                is_generated: metadata.is_generated,
                is_partially_generated: metadata.is_partially_generated,
            })
        }
        // If the Multiplexer hit an error, then it's worth handling the Cancelled case
        // separately: Cancelled means our Multiplexer noted that one of its readers
        // stopped reading. Usually, this will be because one of the readers failed. So,
        // let's just poll the readers once to see if they have an error value ready, and
        // if so, let's return that Error (because it'll be a more usable one). If not,
        // we'll passthrough the cancellation (but, we do have a unit test to make sure we
        // hit the happy path that prettifies the error).
        Err(m @ MultiplexerError::Cancelled) => match futures::poll!(futs) {
            Poll::Ready(Err(e)) => Err(e),
            _ => Err(InternalError(content_id, m.into())),
        },

        Err(m @ MultiplexerError::InputError(..)) => Err(InternalError(content_id, m.into())),
    }
}
