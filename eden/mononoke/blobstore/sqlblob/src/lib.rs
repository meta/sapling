/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg_attr(not(fbcode_build), allow(unused_crate_dependencies))]

mod delay;
#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod myadmin_delay_dummy;
mod store;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use anyhow::format_err;
use async_trait::async_trait;
use base64::Engine;
use base64::alphabet::STANDARD;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::GeneralPurpose;
use base64::engine::general_purpose::NO_PAD;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::BlobstoreMetadata;
use blobstore::CountedBlobstore;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use bytes::Bytes;
use bytes::BytesMut;
use cached_config::ConfigHandle;
use cached_config::ConfigStore;
use cached_config::ModificationTime;
use cached_config::TestSource;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::TryFutureExt;
use futures::stream::FuturesOrdered;
use futures::stream::FuturesUnordered;
use futures::stream::Stream;
use futures::stream::TryStreamExt;
use mononoke_types::BlobstoreBytes;
use mononoke_types::hash::Context as HashContext;
use nonzero_ext::nonzero;
use sql::rusqlite::Connection as SqliteConnection;
use sql_ext::Connection;
use sql_ext::SqlConnections;
use sql_ext::SqlShardedConnections;
use sql_ext::facebook::MysqlOptions;
use sql_ext::facebook::create_mysql_connections_sharded;
use sql_ext::facebook::create_mysql_connections_unsharded;
use sql_ext::open_sqlite_in_memory;
use sql_ext::open_sqlite_path;
use tokio::task::spawn_blocking;
use vec1::Vec1;
use xdb_gc_structs::XdbGc;

use crate::delay::BlobDelay;
#[cfg(fbcode_build)]
use crate::facebook::myadmin_delay;
#[cfg(not(fbcode_build))]
use crate::myadmin_delay_dummy as myadmin_delay;
use crate::store::ChunkGenerationState;
use crate::store::ChunkSqlStore;
use crate::store::ChunkingMethod;
use crate::store::DataSqlStore;
pub use crate::store::SqlblobKeyPage;
// Bring back the pre 0.20 bevahiour and allow either padded or un-padded base64 strings at decode time.
const STANDARD_NO_PAD_INDIFFERENT: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    NO_PAD.with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

// Leaving some space for metadata
const MAX_KEY_SIZE: usize = 200;
// MySQL wants multiple chunks, each around 1 MiB, as a tradeoff between query latency and replication lag
const CHUNK_SIZE: usize = 1024 * 1024;
const SQLITE_SHARD_NUM: NonZeroUsize = nonzero!(2_usize);
const SINGLE_SHARD_NUM: NonZeroUsize = nonzero!(1_usize);
const GC_GENERATION_PATH: &str = "scm/mononoke/xdb_gc/default";

const SQLBLOB_LABEL: &str = "blobstore";

// Test setup data
const UPDATE_FREQUENCY: Duration = Duration::from_millis(1);
const INITIAL_VERSION: u64 = 0;

const COUNTED_ID: &str = "sqlblob";
pub type CountedSqlblob = CountedBlobstore<Sqlblob>;

pub struct Sqlblob {
    data_store: Arc<DataSqlStore>,
    chunk_store: Arc<ChunkSqlStore>,
    put_behaviour: PutBehaviour,
    allow_inline_put: bool,
    ctime_inline_grace: i64,
    mysql_tier: String,
}

impl std::fmt::Display for Sqlblob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(format!("Sqlblob=\"{}\"", self.mysql_tier).as_str())
    }
}

fn get_gc_config_handle(config_store: &ConfigStore) -> Result<ConfigHandle<XdbGc>> {
    config_store.get_config_handle(GC_GENERATION_PATH.to_string())
}

const DEFAULT_ALLOW_INLINE_PUT: bool = true;

// One day
const DEFAULT_CTIME_INLINE_GRACE: i64 = 86400;

// base64 encoding for inline hash has an overhead
pub const MAX_INLINE_LEN: u64 = 255 * 3 / 4;

fn encode_small_value(raw: &[u8]) -> String {
    STANDARD_NO_PAD_INDIFFERENT.encode(raw)
}

impl Sqlblob {
    pub async fn with_mysql(
        fb: FacebookInit,
        shardmap: String,
        shard_num: NonZeroUsize,
        mysql_options: MysqlOptions,
        readonly: bool,
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
    ) -> Result<CountedSqlblob, Error> {
        let delay = if readonly {
            BlobDelay::dummy(shard_num)
        } else {
            myadmin_delay::sharded(fb, shardmap.clone(), shard_num)?
        };
        let config_handle = get_gc_config_handle(config_store)?;
        let shard_count = shard_num.clone().get();

        let SqlShardedConnections {
            read_connections,
            read_master_connections,
            write_connections,
        } = spawn_blocking({
            let shardmap = shardmap.clone();
            move || {
                create_mysql_connections_sharded(
                    fb,
                    mysql_options,
                    SQLBLOB_LABEL.into(),
                    shardmap,
                    0..shard_count,
                    readonly,
                )
            }
        })
        .await??;

        let write_connections = Arc::new(write_connections);
        let read_connections = Arc::new(read_connections);
        let read_master_connections = Arc::new(read_master_connections);
        Ok(Self::counted(
            Self {
                data_store: Arc::new(DataSqlStore::new(
                    shard_num,
                    write_connections.clone(),
                    read_connections.clone(),
                    read_master_connections.clone(),
                    delay.clone(),
                )),
                chunk_store: Arc::new(ChunkSqlStore::new(
                    shard_num,
                    write_connections,
                    read_connections,
                    read_master_connections,
                    delay,
                    config_handle,
                )),
                put_behaviour,
                allow_inline_put: DEFAULT_ALLOW_INLINE_PUT,
                ctime_inline_grace: DEFAULT_CTIME_INLINE_GRACE,
                mysql_tier: shardmap.clone(),
            },
            shardmap,
        ))
    }

    pub async fn with_mysql_unsharded(
        fb: FacebookInit,
        db_address: String,
        mysql_options: MysqlOptions,
        readonly: bool,
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
    ) -> Result<CountedSqlblob, Error> {
        let delay = if readonly {
            BlobDelay::dummy(SINGLE_SHARD_NUM)
        } else {
            myadmin_delay::single(fb, db_address.clone())?
        };
        Self::with_connection_factory(
            delay,
            db_address.clone(),
            SINGLE_SHARD_NUM,
            put_behaviour,
            move |_shard_id| {
                let res = create_mysql_connections_unsharded(
                    fb,
                    mysql_options.clone(),
                    SQLBLOB_LABEL.into(),
                    db_address.clone(),
                    readonly,
                );
                async { res }
            },
            config_store,
            DEFAULT_ALLOW_INLINE_PUT,
            DEFAULT_CTIME_INLINE_GRACE,
        )
        .await
    }

    async fn with_connection_factory<CF, SF>(
        delay: BlobDelay,
        label: String,
        shard_num: NonZeroUsize,
        put_behaviour: PutBehaviour,
        connection_factory: CF,
        config_store: &ConfigStore,
        allow_inline_put: bool,
        ctime_inline_grace: i64,
    ) -> Result<CountedSqlblob, Error>
    where
        CF: Fn(usize) -> SF,
        SF: Future<Output = Result<SqlConnections, Error>> + Sized,
    {
        let shard_count = shard_num.get();

        let config_handle = get_gc_config_handle(config_store)?;

        let futs: FuturesOrdered<_> = (0..shard_count).map(connection_factory).collect();

        let shard_connections = futs.try_collect::<Vec<_>>().await?;
        let mut write_connections = Vec::with_capacity(shard_count);
        let mut read_connections = Vec::with_capacity(shard_count);
        let mut read_master_connections = Vec::with_capacity(shard_count);

        for connections in shard_connections {
            write_connections.push(connections.write_connection);
            read_connections.push(connections.read_connection);
            read_master_connections.push(connections.read_master_connection);
        }

        let write_connections: Arc<Vec1<Connection>> = Arc::new(write_connections.try_into()?);
        let read_connections: Arc<Vec1<Connection>> = Arc::new(read_connections.try_into()?);
        let read_master_connections: Arc<Vec1<Connection>> =
            Arc::new(read_master_connections.try_into()?);

        Ok(Self::counted(
            Self {
                data_store: Arc::new(DataSqlStore::new(
                    shard_num,
                    write_connections.clone(),
                    read_connections.clone(),
                    read_master_connections.clone(),
                    delay.clone(),
                )),
                chunk_store: Arc::new(ChunkSqlStore::new(
                    shard_num,
                    write_connections,
                    read_connections,
                    read_master_connections,
                    delay,
                    config_handle,
                )),
                put_behaviour,
                allow_inline_put,
                ctime_inline_grace,
                mysql_tier: label.clone(),
            },
            label,
        ))
    }

    pub fn with_sqlite_in_memory(
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
        allow_inline_put: bool,
        ctime_inline_grace: i64,
    ) -> Result<CountedSqlblob> {
        Self::with_sqlite(
            put_behaviour,
            |_| {
                let con = open_sqlite_in_memory()?;
                con.execute_batch(Self::CREATION_QUERY)?;
                Ok(con)
            },
            config_store,
            allow_inline_put,
            ctime_inline_grace,
        )
    }

    pub fn with_sqlite_path<P: Into<PathBuf>>(
        path: P,
        readonly_storage: bool,
        put_behaviour: PutBehaviour,
        config_store: &ConfigStore,
    ) -> Result<CountedSqlblob> {
        let pathbuf = path.into();
        Self::with_sqlite(
            put_behaviour,
            move |shard_id| {
                let con = open_sqlite_path(
                    pathbuf.join(format!("shard_{shard_id}.sqlite")),
                    readonly_storage,
                )?;
                con.execute_batch(Self::CREATION_QUERY)?;
                Ok(con)
            },
            config_store,
            DEFAULT_ALLOW_INLINE_PUT,
            DEFAULT_CTIME_INLINE_GRACE,
        )
    }

    fn with_sqlite<F>(
        put_behaviour: PutBehaviour,
        mut constructor: F,
        config_store: &ConfigStore,
        allow_inline_put: bool,
        ctime_inline_grace: i64,
    ) -> Result<CountedSqlblob>
    where
        F: FnMut(usize) -> Result<SqliteConnection>,
    {
        let mut cons = Vec::with_capacity(SQLITE_SHARD_NUM.get());

        for i in 0..SQLITE_SHARD_NUM.get() {
            cons.push(Connection::with_sqlite(constructor(i)?)?);
        }

        let cons: Arc<Vec1<Connection>> = Arc::new(cons.try_into()?);

        // SQLite is predominately intended for tests, and has less concurrency
        // issues relating to GC, so cope with missing configerator
        let config_handle = get_gc_config_handle(config_store)
            .or_else(|_| get_gc_config_handle(&(get_test_config_store().1)))?;

        Ok(Self::counted(
            Self {
                data_store: Arc::new(DataSqlStore::new(
                    SQLITE_SHARD_NUM,
                    cons.clone(),
                    cons.clone(),
                    cons.clone(),
                    BlobDelay::dummy(SQLITE_SHARD_NUM),
                )),
                chunk_store: Arc::new(ChunkSqlStore::new(
                    SQLITE_SHARD_NUM,
                    cons.clone(),
                    cons.clone(),
                    cons,
                    BlobDelay::dummy(SQLITE_SHARD_NUM),
                    config_handle,
                )),
                put_behaviour,
                allow_inline_put,
                ctime_inline_grace,
                mysql_tier: "sqlite".into(),
            },
            "sqlite".into(),
        ))
    }

    const CREATION_QUERY: &'static str = include_str!("../schema/sqlite-sqlblob.sql");

    fn counted(self, label: String) -> CountedBlobstore<Self> {
        CountedBlobstore::new(format!("{COUNTED_ID}.{label}"), self)
    }

    #[cfg(test)]
    pub(crate) fn get_data_store(&self) -> &DataSqlStore {
        &self.data_store
    }

    pub fn get_keys_from_shard(
        &self,
        ctx: &CoreContext,
        shard_num: usize,
    ) -> impl Stream<Item = Result<String>> + use<> {
        self.data_store.get_keys_from_shard(ctx.clone(), shard_num)
    }

    /// Number of SQL shards keys are spread across.
    pub fn shard_count(&self) -> usize {
        self.data_store.shard_count()
    }

    /// Lists one page of the keys in shard `shard_num` that start with `prefix`,
    /// reading at most `limit` rows from the primary.  Pages continue from
    /// `after`, the previous page's `resume_after`.  A page may be empty and
    /// still have a `resume_after`.
    pub async fn get_keys_with_prefix_from_shard(
        &self,
        ctx: &CoreContext,
        shard_num: usize,
        prefix: &str,
        after: Option<&str>,
        limit: NonZeroU32,
    ) -> Result<SqlblobKeyPage> {
        self.data_store
            .get_keys_with_prefix_from_shard(ctx, shard_num, prefix, after, limit)
            .await
    }

    /// Returns a HashMap from generation->(size, chunk_count)
    pub async fn get_chunk_sizes_by_generation(
        &self,
        ctx: &CoreContext,
        shard_num: usize,
    ) -> Result<HashMap<Option<u64>, (u64, u64)>> {
        self.chunk_store
            .get_chunk_sizes_by_generation(ctx, shard_num)
            .await
    }

    pub async fn set_initial_generation(&self, ctx: &CoreContext, shard_num: usize) -> Result<()> {
        self.chunk_store
            .set_initial_generation(ctx, shard_num)
            .await
    }

    #[cfg(test)]
    pub async fn get_chunk_generations(
        &self,
        ctx: &CoreContext,
        key: &str,
    ) -> Result<Vec<Option<u64>>> {
        let chunked = self.data_store.get(ctx, key).await?;
        if let Some(chunked) = chunked {
            let fetch_chunk_generations: FuturesOrdered<_> = (0..chunked.count)
                .map(|chunk_num| {
                    self.chunk_store.get_generation(
                        ctx,
                        &chunked.id,
                        chunk_num,
                        chunked.chunking_method,
                    )
                })
                .collect();
            fetch_chunk_generations.try_collect().await
        } else {
            bail!("key does not exist");
        }
    }

    pub fn get_mark_generation(&self) -> u64 {
        self.chunk_store.get_mark_generation()
    }

    /// Mark the generation for a key
    /// If its value was small enough to inline, then also inline it if requested
    pub async fn set_generation(
        &self,
        ctx: &CoreContext,
        key: &str,
        inline_small_values: bool,
        // Take the mark generation as param, so that marking for an entire run is consistent
        mark_generation: u64,
    ) -> Result<()> {
        let chunked = self.data_store.get(ctx, key).await?;
        if let Some(chunked) = chunked {
            let set_chunk_generations: FuturesUnordered<_> = (0..chunked.count)
                .map(|chunk_num| {
                    self.chunk_store
                        .set_generation(
                            ctx,
                            &chunked.id,
                            chunk_num,
                            chunked.chunking_method,
                            mark_generation,
                        )
                        .map_ok(|value_len| {
                            if let Some(value_len) = value_len {
                                // Should not be chunked at all, request the key is inlined
                                // We do this after marking rather than short circuiting the above
                                // so that inlining following this call fails the key is still live
                                value_len <= MAX_INLINE_LEN
                            } else {
                                false
                            }
                        })
                })
                .collect();
            let can_inline: Vec<bool> = set_chunk_generations.try_collect().await?;
            if inline_small_values && can_inline.len() == 1 && can_inline[0] {
                // Value was small, so lets inline it
                let small_value = self.get_impl(ctx, key).await?;
                if let Some(small_value) = small_value {
                    // Double check length in case it changed since setting generation
                    let value_len: u64 = small_value.as_bytes().len().try_into()?;
                    if value_len <= MAX_INLINE_LEN {
                        if let Some(old_ctime) = small_value.as_meta().ctime() {
                            let ctime = {
                                match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                                    Ok(offset) => offset.as_secs().try_into(),
                                    Err(negative) => {
                                        negative.duration().as_secs().try_into().map(|v: i64| -v)
                                    }
                                }
                            }?;
                            // Optimistic update to convert to the inline form. This only updates for actually old ctimes
                            // (one day or more), so that we don't attempt to inline any new data from e.g. a packer write
                            if ctime - old_ctime >= self.ctime_inline_grace {
                                let small_value = encode_small_value(&small_value.into_raw_bytes());
                                self.data_store
                                    .update_optimistic(
                                        ctx,
                                        key,
                                        ctime,
                                        &small_value,
                                        0,
                                        ChunkingMethod::InlineBase64,
                                        old_ctime,
                                    )
                                    .await?;
                            }
                        }
                    }
                }
            }
            Ok(())
        } else {
            println!("Key {key} has no associated data chunk"); //TODO delete the key without content
            Ok(())
        }
    }

    async fn get_impl<'a>(
        &'a self,
        ctx: &CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        let chunked = self.data_store.get(ctx, key).await?;
        if let Some(chunked) = chunked {
            let blob = match chunked.chunking_method {
                ChunkingMethod::InlineBase64 => {
                    let decoded = STANDARD_NO_PAD_INDIFFERENT.decode(&chunked.id)?;
                    Bytes::copy_from_slice(decoded.as_ref())
                }
                ChunkingMethod::ByContentHashBlake2 => {
                    let chunks = (0..chunked.count)
                        .map(|chunk_num| {
                            self.chunk_store.get(
                                ctx,
                                &chunked.id,
                                chunk_num,
                                chunked.chunking_method,
                            )
                        })
                        .collect::<FuturesOrdered<_>>()
                        .try_collect::<Vec<_>>()
                        .await?;

                    let size = chunks.iter().map(|chunk| chunk.len()).sum();
                    let mut blob = BytesMut::with_capacity(size);
                    for chunk in chunks {
                        blob.extend_from_slice(&chunk);
                    }
                    blob.freeze()
                }
            };

            let meta = BlobstoreMetadata::new(Some(chunked.ctime), None);
            Ok(Some(BlobstoreGetData::new(
                meta,
                BlobstoreBytes::from_bytes(blob),
            )))
        } else {
            Ok(None)
        }
    }
}

impl fmt::Debug for Sqlblob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sqlblob").finish()
    }
}

#[async_trait]
impl Blobstore for Sqlblob {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        self.get_impl(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        if key.len() > MAX_KEY_SIZE {
            return Err(format_err!(
                "Key {key} exceeded max key size {MAX_KEY_SIZE}"
            ));
        }

        if put_behaviour == PutBehaviour::IfAbsent && self.data_store.is_present(ctx, &key).await? {
            // Can short circuit here as key already exists, and is keeping its chunks live
            return Ok(OverwriteStatus::Prevented);
        }

        let value_len: u64 = value.len().try_into()?;

        let chunking_method = if self.allow_inline_put && value_len <= MAX_INLINE_LEN {
            ChunkingMethod::InlineBase64
        } else {
            ChunkingMethod::ByContentHashBlake2
        };

        let put_fut = async {
            let ctime = {
                match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(offset) => offset.as_secs().try_into(),
                    Err(negative) => negative.duration().as_secs().try_into().map(|v: i64| -v),
                }
            }?;
            let (chunk_key, chunk_count, chunk_gen_insert_shard_id) = match chunking_method {
                ChunkingMethod::ByContentHashBlake2 => {
                    let chunk_key = {
                        let mut hash_context = HashContext::new(b"sqlblob");
                        hash_context.update(value.as_bytes());
                        hash_context.finish().to_hex().to_string()
                    };
                    let chunks = value.as_bytes().chunks(CHUNK_SIZE);
                    let chunk_count = chunks.len().try_into()?;
                    let mut updated_gen = false;
                    let mut chunk_gen_insert_shard_id = None;
                    for (chunk_num, chunk_value) in chunks.enumerate() {
                        let chunk_gen_state = self
                            .chunk_store
                            .put(
                                ctx,
                                chunk_key.as_str(),
                                chunk_num.try_into()?,
                                chunking_method,
                                chunk_value,
                                value_len,
                            )
                            .await?;
                        #[expect(
                            clippy::collapsible_match,
                            reason = "turning the inner `if` into a guard would leave the match non-exhaustive"
                        )]
                        match chunk_gen_state {
                            Some(ChunkGenerationState::Updated) => {
                                updated_gen = true;
                                // Seen an update, no need to insert later
                                chunk_gen_insert_shard_id = None;
                            }
                            Some(ChunkGenerationState::NeedsInsertToShard(shard_id)) => {
                                if !updated_gen {
                                    chunk_gen_insert_shard_id = Some(shard_id);
                                }
                            }
                            None => {}
                        };
                    }
                    (chunk_key, chunk_count, chunk_gen_insert_shard_id)
                }
                ChunkingMethod::InlineBase64 => {
                    (encode_small_value(value.as_bytes().as_ref()), 0, None)
                }
            };

            self.data_store
                .put(
                    ctx,
                    &key,
                    ctime,
                    chunk_key.as_str(),
                    chunk_count,
                    chunking_method,
                )
                .await?;

            // Called after data_store.put to maintain invariant that chunk and data put complete
            // successfully before a generation is inserted (aka no dangling generations)
            if let Some(shard_id) = chunk_gen_insert_shard_id {
                self.chunk_store
                    .put_chunk_generation(ctx, &chunk_key, shard_id, value_len)
                    .await?
            }

            Ok(OverwriteStatus::NotChecked)
        };

        match put_behaviour {
            PutBehaviour::Overwrite => put_fut.await,
            PutBehaviour::IfAbsent | PutBehaviour::OverwriteAndLog => {
                match self.data_store.get(ctx, &key).await? {
                    None => {
                        put_fut.await?;
                        Ok(OverwriteStatus::New)
                    }
                    Some(chunked) => {
                        if put_behaviour.should_overwrite() {
                            put_fut.await?;
                            Ok(OverwriteStatus::Overwrote)
                        } else {
                            let chunk_count = chunked.count;
                            for chunk_num in 0..chunk_count {
                                self.chunk_store
                                    .update_generation(
                                        ctx,
                                        &chunked.id,
                                        chunk_num,
                                        chunked.chunking_method,
                                        value_len,
                                    )
                                    .await?;
                            }
                            Ok(OverwriteStatus::Prevented)
                        }
                    }
                }
            }
        }
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.put_explicit(ctx, key, value, self.put_behaviour).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        let present = self.data_store.is_present(ctx, key).await?;
        Ok(if present {
            BlobstoreIsPresent::Present
        } else {
            BlobstoreIsPresent::Absent
        })
    }

    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        let existing_data = self
            .data_store
            .get(ctx, old_key)
            .await?
            .ok_or_else(|| format_err!("Key {old_key} does not exist in the blobstore"))?;
        self.data_store
            .put(
                ctx,
                &new_key,
                existing_data.ctime,
                &existing_data.id,
                existing_data.count,
                existing_data.chunking_method,
            )
            .await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        if !self.data_store.is_present(ctx, key).await? {
            bail!("Sqlblob::unlink: key {key} does not exist in the blobstore")
        };
        self.data_store.unlink(ctx, key).await
    }
}

pub fn set_test_generations(
    source: &TestSource,
    put_generation: i64,
    mark_generation: i64,
    delete_generation: i64,
    mod_time: u64,
) -> u64 {
    source.insert_config(
        GC_GENERATION_PATH,
        &serde_json::to_string(&XdbGc {
            put_generation,
            mark_generation,
            delete_generation,
        })
        .expect("Invalid input config somehow"),
        ModificationTime::UnixTimestamp(mod_time),
    );
    source.insert_to_refresh(GC_GENERATION_PATH.to_string());
    mark_generation as u64
}

pub fn get_test_config_store() -> (Arc<TestSource>, ConfigStore) {
    let test_source = Arc::new(TestSource::new());
    set_test_generations(test_source.as_ref(), 2, 1, 0, INITIAL_VERSION);
    (
        test_source.clone(),
        ConfigStore::new(test_source, UPDATE_FREQUENCY, None),
    )
}
