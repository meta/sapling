/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::Loadable;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;

use crate::ContentId;
use crate::FileContents;
use crate::audit::LoadError;
use crate::audit::TaggedContext;
use crate::audit::Validatable;

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
