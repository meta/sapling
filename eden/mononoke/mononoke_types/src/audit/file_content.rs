/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::Loadable;
use fbthrift::compact_protocol;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;

use crate::Alias;
use crate::BlobstoreKey;
use crate::ContentId;
use crate::ContentMetadataV2;
use crate::ContentMetadataV2Id;
use crate::FileContents;
use crate::audit::LoadError;
use crate::audit::TaggedContext;
use crate::audit::Validatable;
use crate::errors::MononokeTypeError;
use crate::hash::Blake3;
use crate::hash::RichGitSha1;
use crate::hash::Sha1;
use crate::hash::Sha256;
use crate::thrift;

/// As in `filestore::fetch`, the chunks of a file loaded at once total at most
/// this many bytes.
const CHUNK_LOAD_BUDGET: u64 = 16 * 1024 * 1024;

/// Unlike `filestore::fetch`, which reads one file, the chunk loads in flight
/// are also capped by count, since many contents are checked at once.
const MAX_CHUNK_LOADS: usize = 64;

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for ContentId {
    const NAME: &'static str = "FileContent";

    /// Number of content bytes read.
    type Loaded = u64;

    // Keeps only the chunks' sizes, not their bytes: files can be arbitrarily
    // large.
    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<u64, LoadError> {
        let blobstore = ctx.blobstore();
        let chunks = match Loadable::load(self, ctx, blobstore).await? {
            FileContents::Bytes(bytes) => return Ok(bytes.len() as u64),
            FileContents::Chunked(chunked) => chunked.into_chunks(),
        };
        let largest = chunks.iter().map(|chunk| chunk.size()).max().unwrap_or(0);
        stream::iter(chunks)
            .map(move |pointer| async move {
                let chunk_id = pointer.chunk_id();
                let chunk = Loadable::load(&chunk_id, ctx, blobstore).await?;
                let size = chunk.into_bytes().len() as u64;
                if size != pointer.size() {
                    return Err(LoadError::Unreadable(anyhow!(
                        "chunk {chunk_id} holds {size} bytes, not the {} its file's chunk list records",
                        pointer.size()
                    )));
                }
                Ok(size)
            })
            .buffer_unordered(chunk_loads_at_once(largest))
            .try_fold(0, |size, chunk| async move { Ok(size + chunk) })
            .await
    }

    fn storage_key(&self, _repo: &R) -> Option<String> {
        Some(self.blobstore_key())
    }
}

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for ContentMetadataV2Id {
    const NAME: &'static str = "FileContentMetadataV2";

    type Loaded = ContentMetadataV2;

    // The read of `filestore::get_metadata_readonly`. `core` reads metadata
    // while enumerating instead, with `load_stored_metadata`.
    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<ContentMetadataV2, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }

    fn storage_key(&self, _repo: &R) -> Option<String> {
        Some(self.blobstore_key())
    }
}

/// An alias mapping. Loading it does not follow it back to its content.
#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for Alias {
    const NAME: &'static str = "AliasContentMapping";

    /// The content the alias maps to.
    type Loaded = ContentId;

    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<ContentId, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }

    fn storage_key(&self, _repo: &R) -> Option<String> {
        Some(self.blobstore_key())
    }
}

/// Content metadata as stored.
pub(crate) enum StoredMetadata {
    Current(ContentMetadataV2),
    /// Metadata stored before spring 2023, which production readers recompute.
    OldFormat(OldFormatMetadata),
}

/// Loads stored metadata as `get_metadata_readonly` does, except that a
/// record whose only decode problem is that it lacks some of the fields added
/// in spring 2023 (`is_generated`, `is_partially_generated` and
/// `seeded_blake3`) is returned as old format instead of failing. The bytes
/// are read once and decoded as `Loadable` decodes them, so the record that
/// failed to decode is the one checked as old format.
pub(crate) async fn load_stored_metadata(
    id: &ContentMetadataV2Id,
    ctx: &TaggedContext,
) -> Result<StoredMetadata, LoadError> {
    let key = id.blobstore_key();
    let bytes = ctx
        .blobstore()
        .get(ctx, &key)
        .await?
        .ok_or(LoadError::Missing(key))?
        .into_raw_bytes();
    let error = match ContentMetadataV2::from_bytes(&bytes) {
        Ok(metadata) => return Ok(StoredMetadata::Current(metadata)),
        Err(error) => error,
    };
    // `get_metadata` recomputes on the same error.
    if !matches!(
        error.downcast_ref::<MononokeTypeError>(),
        Some(MononokeTypeError::InvalidThrift(..))
    ) {
        return Err(LoadError::Unreadable(error));
    }
    OldFormatMetadata::decode(&bytes)
        .map(StoredMetadata::OldFormat)
        .ok_or(LoadError::Unreadable(error))
}

/// What an old-format record says about its content.
pub(crate) struct OldFormatMetadata {
    content_id: ContentId,
    total_size: u64,
    sha1: Sha1,
    sha256: Sha256,
    git_sha1: RichGitSha1,
    /// Absent from most old records.
    seeded_blake3: Option<Blake3>,
}

impl OldFormatMetadata {
    /// Decodes `bytes` if they lack some of the fields added in spring 2023
    /// and decode once those are filled in.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut record: thrift::content::ContentMetadataV2 =
            compact_protocol::deserialize(bytes).ok()?;
        if record.is_generated.is_some()
            && record.is_partially_generated.is_some()
            && record.seeded_blake3.is_some()
        {
            return None;
        }
        let seeded_blake3 = record.seeded_blake3.is_some();
        // Placeholders for the absent fields, never compared, so that the
        // rest decodes and is validated as in any record.
        record.is_generated.get_or_insert(false);
        record.is_partially_generated.get_or_insert(false);
        record
            .seeded_blake3
            .get_or_insert_with(|| Blake3::from_byte_array([0; 32]).into_thrift());
        let decoded = ContentMetadataV2::from_thrift(record).ok()?;
        Some(Self {
            content_id: decoded.content_id,
            total_size: decoded.total_size,
            sha1: decoded.sha1,
            sha256: decoded.sha256,
            git_sha1: decoded.git_sha1,
            seeded_blake3: seeded_blake3.then_some(decoded.seeded_blake3),
        })
    }

    /// Fails as unreadable unless the content, its size and every hash the
    /// record holds are those recomputed from the content.
    pub(crate) fn check(&self, recomputed: &ContentMetadataV2) -> Result<(), LoadError> {
        let differing = [
            (
                "content_id",
                self.content_id.to_string(),
                recomputed.content_id.to_string(),
            ),
            (
                "total_size",
                self.total_size.to_string(),
                recomputed.total_size.to_string(),
            ),
            ("sha1", self.sha1.to_string(), recomputed.sha1.to_string()),
            (
                "sha256",
                self.sha256.to_string(),
                recomputed.sha256.to_string(),
            ),
            (
                "git_sha1",
                self.git_sha1.to_string(),
                recomputed.git_sha1.to_string(),
            ),
        ]
        .into_iter()
        .chain(self.seeded_blake3.map(|seeded_blake3| {
            (
                "seeded_blake3",
                seeded_blake3.to_string(),
                recomputed.seeded_blake3.to_string(),
            )
        }))
        .filter(|(_field, stored, recomputed)| stored != recomputed)
        .map(|(field, stored, recomputed)| format!("{field} {stored}, not {recomputed}"))
        .collect::<Vec<_>>();
        if differing.is_empty() {
            return Ok(());
        }
        Err(LoadError::Unreadable(anyhow!(
            "old-format metadata does not match its content: it holds {}",
            differing.join(", ")
        )))
    }
}

/// How many chunks of a file are loaded at once, given its largest chunk.
fn chunk_loads_at_once(largest_chunk: u64) -> usize {
    let within_budget = CHUNK_LOAD_BUDGET / largest_chunk.max(1);
    usize::try_from(within_budget)
        .unwrap_or(usize::MAX)
        .clamp(1, MAX_CHUNK_LOADS)
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn chunk_loads_stay_within_the_byte_budget_and_the_load_cap() {
        // 4 MiB chunks: the 16 MiB budget allows four at once.
        assert_eq!(chunk_loads_at_once(4 * 1024 * 1024), 4);
        // A chunk larger than the budget is still loaded, one at a time.
        assert_eq!(chunk_loads_at_once(32 * 1024 * 1024), 1);
        // Tiny chunks: the budget alone would allow millions at once.
        assert_eq!(chunk_loads_at_once(1), MAX_CHUNK_LOADS);
        assert_eq!(chunk_loads_at_once(0), MAX_CHUNK_LOADS);
    }
}
