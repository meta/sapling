/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::hash::Hasher;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Error;
use anyhow::bail;
use anyhow::format_err;
use bytes::BytesMut;
use cached_config::ConfigHandle;
use context::CoreContext;
use futures::future::TryFutureExt;
use futures::stream;
use futures::stream::Stream;
use sql::Binary;
use sql_ext::Connection;
use sql_ext::mononoke_queries;
use twox_hash::XxHash32;
use vec1::Vec1;
use xdb_gc_structs::XdbGc;

use crate::delay::BlobDelay;

mod types {
    use sql::mysql;
    use sql::mysql_async::FromValueError;
    use sql::mysql_async::Value;
    use sql::mysql_async::prelude::FromValue;

    type FromValueResult<T> = Result<T, FromValueError>;

    #[derive(Clone, Copy, Debug, PartialEq, mysql::OptTryFromRowField)]
    pub enum ChunkingMethod {
        ByContentHashBlake2,
        InlineBase64,
    }

    impl From<ChunkingMethod> for Value {
        fn from(dtype: ChunkingMethod) -> Self {
            match dtype {
                // When you add here, please add the reverse transform
                // to impl ConvIr<ChunkingMethod> below
                ChunkingMethod::ByContentHashBlake2 => Value::UInt(1),
                ChunkingMethod::InlineBase64 => Value::UInt(2),
            }
        }
    }

    impl TryFrom<Value> for ChunkingMethod {
        type Error = FromValueError;
        fn try_from(v: Value) -> FromValueResult<Self> {
            match v {
                // Note that every value repeats 3 times - integer, unsigned, string - because MySQL can convert to
                // any of those for a response. We normally see UInt, but we want this to be safe against
                // surprises
                Value::Int(1) => Ok(ChunkingMethod::ByContentHashBlake2),
                Value::UInt(1) => Ok(ChunkingMethod::ByContentHashBlake2),
                Value::Bytes(ref b) if b == b"1" => Ok(ChunkingMethod::ByContentHashBlake2),
                Value::Int(2) => Ok(ChunkingMethod::InlineBase64),
                Value::UInt(2) => Ok(ChunkingMethod::InlineBase64),
                Value::Bytes(ref b) if b == b"2" => Ok(ChunkingMethod::InlineBase64),
                // If you need to add to this error path, ensure that the type you are adding cannot be converted to an integer
                // by MySQL
                v @ Value::NULL
                | v @ Value::Bytes(..)
                | v @ Value::Float(..)
                | v @ Value::Double(..)
                | v @ Value::Date(..)
                | v @ Value::Time(..)
                | v @ Value::Int(..)
                | v @ Value::UInt(..) => Err(FromValueError(v)),
            }
        }
    }

    impl FromValue for ChunkingMethod {
        type Intermediate = ChunkingMethod;
    }
}

pub use self::types::ChunkingMethod;

mononoke_queries! {
    write InsertData(values: (id: &str, ctime: i64, chunk_id: &str, chunk_count: u32, chunking_method: ChunkingMethod)) {
        insert_or_ignore,
        "{insert_or_ignore} INTO data (
            id
            , creation_time
            , chunk_id
            , chunk_count
            , chunking_method
        ) VALUES {values}"
    }

    write DeleteData(id: &str) {
        none,
        "DELETE FROM data WHERE id = {id}"
    }

    write UpdateData(id: &str, ctime: i64, chunk_id: &str, chunk_count: u32, chunking_method: ChunkingMethod) {
        none,
        "UPDATE data SET
            creation_time = {ctime}
            , chunk_id = {chunk_id}
            , chunk_count = {chunk_count}
            , chunking_method = {chunking_method}
        WHERE id = {id}"
    }


    write UpdateDataOptimistic(id: &str, ctime: i64, chunk_id: &str, chunk_count: u32, chunking_method: ChunkingMethod, old_ctime: i64) {
        none,
        "UPDATE data SET
            creation_time = {ctime}
            , chunk_id = {chunk_id}
            , chunk_count = {chunk_count}
            , chunking_method = {chunking_method}
        WHERE id = {id} AND creation_time = {old_ctime}"
    }

    write InsertChunk(values: (id: &str, chunk_num: u32, value: Binary<'_>)) {
        insert_or_ignore,
        "{insert_or_ignore} INTO chunk (
            id
            , chunk_num
            , value
        ) VALUES {values}"
    }

    write UpdateGeneration(id: &str, generation: u64, value_len: u64) {
        none,
        "UPDATE chunk_generation
            SET last_seen_generation = {generation}, value_len = {value_len}
            WHERE id = {id} AND last_seen_generation < {generation}"
    }

    read SelectData(id: &str) -> (i64, Vec<u8>, u32, ChunkingMethod) {
        "SELECT creation_time, chunk_id, chunk_count, chunking_method
         FROM data
         WHERE id = {id}"
    }

    read SelectIsDataPresent(id: &str) -> (i32) {
        "SELECT 1
         FROM data
         WHERE id = {id}"
    }

    read SelectChunk(id: &str, chunk_num: u32) -> (Vec<u8>) {
        "SELECT value
         FROM chunk
         WHERE id = {id}
           AND chunk_num = {chunk_num}"
    }

    read SelectChunkLen(id: &str) -> (u64) {
        "SELECT CAST(SUM(LENGTH(value)) AS UNSIGNED)
         FROM chunk
         WHERE id = {id}"
    }

    read GetChunkGeneration(id: &str) -> (u64, u64) {
        "SELECT last_seen_generation, value_len
        FROM chunk_generation
        WHERE id = {id}"
    }

    write InsertGeneration(values: (id: &str, generation: u64, value_len: u64)) {
        insert_or_ignore,
        "{insert_or_ignore} INTO chunk_generation VALUES {values}"
    }

    read GetNeedsInitialGeneration(limit: u64) -> (Vec<u8>, Option<u64>) {
        "SELECT chunk.id, chunk_generation.value_len
        FROM chunk LEFT JOIN chunk_generation ON chunk.id = chunk_generation.id
        WHERE chunk_generation.last_seen_generation IS NULL
        LIMIT {limit}"
    }

    read GetAllKeys() -> (Vec<u8>) {
        "SELECT id FROM data"
    }

    read GetGenerationSizes() -> (Option<u64>, u64, u64) {
        "SELECT chunk_generation.last_seen_generation, CAST(SUM(chunk_generation.value_len) AS UNSIGNED), CAST(COUNT(1) AS UNSIGNED)
        FROM chunk_generation
        GROUP BY chunk_generation.last_seen_generation"
    }
}

pub struct Chunked {
    pub id: String,
    pub count: u32,
    pub ctime: i64,
    pub chunking_method: ChunkingMethod,
}

#[derive(Clone)]
pub(crate) struct DataSqlStore {
    shard_count: NonZeroUsize,
    write_connection: Arc<Vec1<Connection>>,
    read_connection: Arc<Vec1<Connection>>,
    read_master_connection: Arc<Vec1<Connection>>,
    delay: BlobDelay,
}

impl DataSqlStore {
    pub(crate) fn new(
        shard_count: NonZeroUsize,
        write_connection: Arc<Vec1<Connection>>,
        read_connection: Arc<Vec1<Connection>>,
        read_master_connection: Arc<Vec1<Connection>>,
        delay: BlobDelay,
    ) -> Self {
        Self {
            shard_count,
            write_connection,
            read_connection,
            read_master_connection,
            delay,
        }
    }

    pub(crate) async fn get(&self, ctx: &CoreContext, key: &str) -> Result<Option<Chunked>, Error> {
        let shard_id = self.shard(key);

        let rows = {
            let rows = SelectData::query(
                &self.read_connection[shard_id],
                ctx.sql_query_telemetry(),
                &key,
            )
            .await?;
            if rows.is_empty() {
                SelectData::query(
                    &self.read_master_connection[shard_id],
                    ctx.sql_query_telemetry(),
                    &key,
                )
                .await?
            } else {
                rows
            }
        };

        Ok(rows
            .into_iter()
            .next()
            .map(|(ctime, chunk_id, chunk_count, chunking_method)| Chunked {
                id: String::from_utf8_lossy(&chunk_id).to_string(),
                count: chunk_count,
                ctime,
                chunking_method,
            }))
    }

    pub(crate) async fn put(
        &self,
        ctx: &CoreContext,
        key: &str,
        ctime: i64,
        chunk_id: &str,
        chunk_count: u32,
        chunking_method: ChunkingMethod,
    ) -> Result<(), Error> {
        let shard_id = self.shard(key);

        self.delay.delay(shard_id).await;

        let res = InsertData::query(
            &self.write_connection[shard_id],
            ctx.sql_query_telemetry(),
            &[(&key, &ctime, &chunk_id, &chunk_count, &chunking_method)],
        )
        .await?;
        if res.affected_rows() == 0 {
            UpdateData::query(
                &self.write_connection[shard_id],
                ctx.sql_query_telemetry(),
                &key,
                &ctime,
                &chunk_id,
                &chunk_count,
                &chunking_method,
            )
            .await?;
        }
        Ok(())
    }

    // Update optimistically using ctime as the optimistic lock check
    // Used from gc marking to inline small blobs where ctime hasn't changed
    pub(crate) async fn update_optimistic(
        &self,
        ctx: &CoreContext,
        key: &str,
        ctime: i64,
        chunk_id: &str,
        chunk_count: u32,
        chunking_method: ChunkingMethod,
        old_ctime: i64,
    ) -> Result<(), Error> {
        let shard_id = self.shard(key);
        self.delay.delay(shard_id).await;

        UpdateDataOptimistic::query(
            &self.write_connection[shard_id],
            ctx.sql_query_telemetry(),
            &key,
            &ctime,
            &chunk_id,
            &chunk_count,
            &chunking_method,
            &old_ctime,
        )
        .await?;

        Ok(())
    }

    pub(crate) async fn unlink(&self, ctx: &CoreContext, key: &str) -> Result<(), Error> {
        let shard_id = self.shard(key);

        self.delay.delay(shard_id).await;

        // Deleting from data table does not remove the chunks as they are content addressed.  GC checks for orphaned chunks and removes them.
        let res = DeleteData::query(
            &self.write_connection[shard_id],
            ctx.sql_query_telemetry(),
            &key,
        )
        .await?;
        if res.affected_rows() != 1 {
            bail!(
                "Unexpected row_count {} from sqlblob unlink for {}",
                res.affected_rows(),
                key
            );
        }
        Ok(())
    }

    pub(crate) async fn is_present(&self, ctx: &CoreContext, key: &str) -> Result<bool, Error> {
        let shard_id = self.shard(key);

        let rows = {
            let rows = SelectIsDataPresent::query(
                &self.read_connection[shard_id],
                ctx.sql_query_telemetry(),
                &key,
            )
            .await?;
            if rows.is_empty() {
                SelectIsDataPresent::query(
                    &self.read_master_connection[shard_id],
                    ctx.sql_query_telemetry(),
                    &key,
                )
                .await?
            } else {
                rows
            }
        };
        Ok(!rows.is_empty())
    }

    pub(crate) fn get_keys_from_shard(
        &self,
        ctx: CoreContext,
        shard_num: usize,
    ) -> impl Stream<Item = Result<String, Error>> + use<> {
        let conn = self.read_master_connection[shard_num].clone();
        async move {
            let keys = GetAllKeys::query(&conn, ctx.sql_query_telemetry()).await?;
            Ok(stream::iter(
                keys.into_iter()
                    .map(|(id,)| Ok(String::from_utf8_lossy(&id).to_string())),
            ))
        }
        .try_flatten_stream()
    }

    fn shard(&self, key: &str) -> usize {
        let mut hasher = XxHash32::with_seed(0);
        hasher.write(key.as_bytes());
        (hasher.finish() % self.shard_count.get() as u64) as usize
    }
}
pub(crate) enum ChunkGenerationState {
    NeedsInsertToShard(usize),
    Updated,
}

#[derive(Clone)]
pub(crate) struct ChunkSqlStore {
    shard_count: NonZeroUsize,
    write_connection: Arc<Vec1<Connection>>,
    read_connection: Arc<Vec1<Connection>>,
    read_master_connection: Arc<Vec1<Connection>>,
    delay: BlobDelay,
    gc_generations: ConfigHandle<XdbGc>,
}

impl ChunkSqlStore {
    pub(crate) fn new(
        shard_count: NonZeroUsize,
        write_connection: Arc<Vec1<Connection>>,
        read_connection: Arc<Vec1<Connection>>,
        read_master_connection: Arc<Vec1<Connection>>,
        delay: BlobDelay,
        gc_generations: ConfigHandle<XdbGc>,
    ) -> Self {
        Self {
            shard_count,
            write_connection,
            read_connection,
            read_master_connection,
            delay,
            gc_generations,
        }
    }

    pub(crate) async fn get(
        &self,
        ctx: &CoreContext,
        id: &str,
        chunk_num: u32,
        chunking_method: ChunkingMethod,
    ) -> Result<BytesMut, Error> {
        if let Some(shard_id) = self.shard(id, chunk_num, chunking_method) {
            let rows = {
                let rows = SelectChunk::query(
                    &self.read_connection[shard_id],
                    ctx.sql_query_telemetry(),
                    &id,
                    &chunk_num,
                )
                .await?;
                if rows.is_empty() {
                    SelectChunk::query(
                        &self.read_master_connection[shard_id],
                        ctx.sql_query_telemetry(),
                        &id,
                        &chunk_num,
                    )
                    .await?
                } else {
                    rows
                }
            };
            rows.into_iter()
                .next()
                .map(|(value,)| (&*value).into())
                .ok_or_else(|| format_err!("Missing chunk with id {chunk_num} shard {shard_id}"))
        } else {
            bail!(
                "ChunkSqlStore::get() unexpectedly called for inline chunking_method {chunking_method:?}"
            )
        }
    }

    /// Returns the shard and number of chunk_generation rows updated
    pub(crate) async fn put(
        &self,
        ctx: &CoreContext,
        key: &str,
        chunk_num: u32,
        chunking_method: ChunkingMethod,
        value: &[u8],
        full_value_len: u64,
    ) -> Result<Option<ChunkGenerationState>, Error> {
        if let Some(shard_id) = self.shard(key, chunk_num, chunking_method) {
            self.delay.delay(shard_id).await;
            let generation = self.gc_generations.get().put_generation as u64;
            let conn = &self.write_connection[shard_id];
            // Update generation in case it already exists
            let updated = UpdateGeneration::query(
                conn,
                ctx.sql_query_telemetry(),
                &key,
                &generation,
                &full_value_len,
            )
            .await?;
            InsertChunk::query(
                conn,
                ctx.sql_query_telemetry(),
                &[(&key, &chunk_num, &Binary(value))],
            )
            .await?;
            if updated.affected_rows() > 0 {
                Ok(Some(ChunkGenerationState::Updated))
            } else {
                Ok(Some(ChunkGenerationState::NeedsInsertToShard(shard_id)))
            }
        } else {
            Ok(None)
        }
    }

    pub(crate) fn get_mark_generation(&self) -> u64 {
        self.gc_generations.get().mark_generation as u64
    }

    // Store an entry for value_len eagerly if it was missing on ChunkSqlStore::put()'s UpdateGeneration
    // Saves lazy computing it with associated MySQL read bandwidth from length(chunk.value) later.
    pub(crate) async fn put_chunk_generation(
        &self,
        ctx: &CoreContext,
        key: &str,
        shard_id: usize,
        full_value_len: u64,
    ) -> Result<(), Error> {
        let generation = self.gc_generations.get().put_generation as u64;
        let conn = &self.write_connection[shard_id];
        InsertGeneration::query(
            conn,
            ctx.sql_query_telemetry(),
            &[(&key, &generation, &full_value_len)],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn update_generation(
        &self,
        ctx: &CoreContext,
        key: &str,
        chunk_num: u32,
        chunking_method: ChunkingMethod,
        value_len: u64,
    ) -> Result<(), Error> {
        if let Some(shard_id) = self.shard(key, chunk_num, chunking_method) {
            self.delay.delay(shard_id).await;
            UpdateGeneration::query(
                &self.write_connection[shard_id],
                ctx.sql_query_telemetry(),
                &key,
                &(self.gc_generations.get().put_generation as u64),
                &value_len,
            )
            .await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn get_generation(
        &self,
        ctx: &CoreContext,
        key: &str,
        chunk_num: u32,
        chunking_method: ChunkingMethod,
    ) -> Result<Option<u64>, Error> {
        if let Some(shard_id) = self.shard(key, chunk_num, chunking_method) {
            let rows = {
                let rows = GetChunkGeneration::query(
                    &self.read_connection[shard_id],
                    ctx.sql_query_telemetry(),
                    &key,
                )
                .await?;
                if rows.is_empty() {
                    GetChunkGeneration::query(
                        &self.read_master_connection[shard_id],
                        ctx.sql_query_telemetry(),
                        &key,
                    )
                    .await?
                } else {
                    rows
                }
            };
            Ok(rows.into_iter().next().map(|(v, _value_len)| v))
        } else {
            Ok(None)
        }
    }

    async fn get_len(&self, ctx: &CoreContext, shard_id: usize, key: &str) -> Result<u64, Error> {
        let rows = {
            let rows = SelectChunkLen::query(
                &self.read_connection[shard_id],
                ctx.sql_query_telemetry(),
                &key,
            )
            .await?;
            if rows.is_empty() {
                SelectChunkLen::query(
                    &self.read_master_connection[shard_id],
                    ctx.sql_query_telemetry(),
                    &key,
                )
                .await?
            } else {
                rows
            }
        };
        rows.into_iter()
            .next()
            .map(|(value_len,)| value_len)
            .ok_or_else(|| format_err!("Missing chunk with id {key} shard {shard_id}"))
    }

    // Returns length of the chunk value if known
    pub(crate) async fn set_generation(
        &self,
        ctx: &CoreContext,
        key: &str,
        chunk_num: u32,
        chunking_method: ChunkingMethod,
        // Take the mark generation as param, so that marking for an entire run is consistent
        mark_generation: u64,
    ) -> Result<Option<u64>, Error> {
        if let Some(shard_id) = self.shard(key, chunk_num, chunking_method) {
            // Take latest value for put generation
            let put_generation = self.gc_generations.get().put_generation as u64;

            // Short-circuit if we have a generation and that generation is >= mark_generation
            let found_generation = GetChunkGeneration::query(
                &self.read_connection[shard_id],
                ctx.sql_query_telemetry(),
                &key,
            )
            .await?
            .into_iter()
            .next();
            let (found_generation, value_len) =
                if let Some((found_generation, value_len)) = found_generation {
                    if found_generation >= mark_generation {
                        return Ok(Some(value_len));
                    }
                    (Some(found_generation), Some(value_len))
                } else {
                    let found_generation = GetChunkGeneration::query(
                        &self.read_master_connection[shard_id],
                        ctx.sql_query_telemetry(),
                        &key,
                    )
                    .await?
                    .into_iter()
                    .next();

                    if let Some((found_generation, value_len)) = found_generation {
                        if found_generation >= mark_generation {
                            return Ok(Some(value_len));
                        }
                        (Some(found_generation), Some(value_len))
                    } else {
                        (None, None)
                    }
                };

            // Make sure we know how large the value is
            let value_len: u64 = if let Some(value_len) = value_len {
                value_len
            } else {
                // This chunk has never had value_len populated so get it from chunk.value
                self.get_len(ctx, shard_id, key).await?
            };

            // About to start writing so delay
            self.delay.delay(shard_id).await;

            if found_generation.is_none() {
                // First set the generation if unset, so that future writers will update it.
                InsertGeneration::query(
                    &self.write_connection[shard_id],
                    ctx.sql_query_telemetry(),
                    &[(&key, &put_generation, &value_len)],
                )
                .await?;
            }
            // Then update it in case it already existed
            UpdateGeneration::query(
                &self.write_connection[shard_id],
                ctx.sql_query_telemetry(),
                &key,
                &mark_generation,
                &value_len,
            )
            .await?;

            return Ok(Some(value_len));
        }
        Ok(None)
    }

    // Returns a HashMap from generation->(size, chunk_id_count)
    // Its a chunk id count as some chunk ids have multiple chunks of CHUNK_SIZE
    // but chunk_generation doesn't record that (it doesn't need to)
    pub(crate) async fn get_chunk_sizes_by_generation(
        &self,
        ctx: &CoreContext,
        shard_num: usize,
    ) -> Result<HashMap<Option<u64>, (u64, u64)>, Error> {
        GetGenerationSizes::query(
            &self.read_master_connection[shard_num],
            ctx.sql_query_telemetry(),
        )
        .await
        .map(|s| {
            s.into_iter()
                .map(|(r#gen, size, count)| (r#gen, (size, count)))
                .collect::<HashMap<_, (_, _)>>()
        })
    }

    pub(crate) async fn set_initial_generation(
        &self,
        ctx: &CoreContext,
        shard_num: usize,
    ) -> Result<(), Error> {
        loop {
            self.delay.delay(shard_num).await;
            let conn = &self.write_connection[shard_num];
            let chunks_needing_gen =
                GetNeedsInitialGeneration::query(conn, ctx.sql_query_telemetry(), &10000).await?;
            if chunks_needing_gen.is_empty() {
                return Ok(());
            }

            let generation = self.gc_generations.get().put_generation as u64;

            for (id, value_len) in chunks_needing_gen {
                let id = String::from_utf8_lossy(&id);
                let value_len = if let Some(value_len) = value_len {
                    value_len
                } else {
                    self.get_len(ctx, shard_num, &id).await?
                };

                InsertGeneration::query(
                    conn,
                    ctx.sql_query_telemetry(),
                    &[(&id.as_ref(), &generation, &value_len)],
                )
                .await?;
            }
        }
    }

    // Returns None if the value is stored inline without needing chunk table lookup
    fn shard(&self, key: &str, chunk_id: u32, chunking_method: ChunkingMethod) -> Option<usize> {
        match chunking_method {
            ChunkingMethod::InlineBase64 => None,
            ChunkingMethod::ByContentHashBlake2 => {
                let mut hasher = XxHash32::with_seed(0);
                hasher.write(key.as_bytes());
                hasher.write_u32(chunk_id);
                Some((hasher.finish() % self.shard_count.get() as u64) as usize)
            }
        }
    }
}
