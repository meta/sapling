/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::fmt;
use std::iter::zip;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use blobstore_stats::OperationType;
use blobstore_sync_queue::BlobstoreWal;
use blobstore_sync_queue::BlobstoreWalEntry;
use cloned::cloned;
use context::CoreContext;
use context::PerfCounterType;
use fbinit::FacebookInit;
use futures::Future;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream::FuturesUnordered;
use futures_stats::TimedFutureExt;
use metaconfig_types::BlobstoreId;
use metaconfig_types::MultiplexId;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::Timestamp;
use multiplexedblob::scuba;
use scuba_ext::MononokeScubaSampleBuilder;
use thiserror::Error;
use time_ext::DurationExt;
use tokio::task::JoinHandle;

use crate::timed::MultiplexTimeout;
use crate::timed::TimedStore;
use crate::timed::with_timed_stores;
type BlobstoresReturnedError = HashMap<BlobstoreId, Error>;

#[derive(Error, Debug, Clone)]
pub enum MultiplexedWalError {
    #[error("All blobstores failed: {0:?}")]
    AllFailed(Arc<BlobstoresReturnedError>),
    #[error("Failures on put in underlying single blobstores: {0:?}")]
    SomePutsFailed(Arc<BlobstoresReturnedError>),
    #[error("Failures on unlink in underlying single blobstores: {0:?}")]
    SomeUnlinksFailed(Arc<BlobstoresReturnedError>),
    #[error("Failures on get in underlying single blobstores: {0:?}")]
    SomeGetsFailed(Arc<BlobstoresReturnedError>),
    #[error("Failures on is_present in underlying single blobstores: {0:?}")]
    SomeIsPresentsFailed(Arc<BlobstoresReturnedError>),
}

#[derive(Clone, Debug)]
pub struct MultiplexQuorum {
    pub(crate) read: NonZeroUsize,
    pub(crate) write: NonZeroUsize,
}

impl MultiplexQuorum {
    fn new(num_stores: usize, write: usize) -> Result<Self> {
        if write > num_stores {
            return Err(anyhow!(
                "Not enough blobstores for configured put or get needs. Have {num_stores}, need {write} puts",
            ));
        }

        Ok(Self {
            write: NonZeroUsize::new(write).ok_or_else(|| anyhow!("Write quorum cannot be 0"))?,
            read: NonZeroUsize::new(num_stores - write + 1).unwrap(),
        })
    }
}

#[derive(Clone)]
pub struct Scuba {
    pub(crate) inner_blobstores_scuba: MononokeScubaSampleBuilder,
    multiplex_scuba: MononokeScubaSampleBuilder,
    sample_rate: NonZeroU64,
}

impl Scuba {
    pub fn new_from_raw(
        fb: FacebookInit,
        inner_blobstores_scuba_table: Option<String>,
        multiplex_scuba_table: Option<String>,
        sample_rate: NonZeroU64,
    ) -> Result<Self> {
        let inner = inner_blobstores_scuba_table.map_or_else(
            || Ok(MononokeScubaSampleBuilder::with_discard()),
            |table| MononokeScubaSampleBuilder::new(fb, &table),
        )?;
        let multiplex = multiplex_scuba_table.map_or_else(
            || Ok(MononokeScubaSampleBuilder::with_discard()),
            |table| MononokeScubaSampleBuilder::new(fb, &table),
        )?;

        Self::new(inner, multiplex, sample_rate)
    }

    pub fn new(
        mut inner_blobstores_scuba: MononokeScubaSampleBuilder,
        mut multiplex_scuba: MononokeScubaSampleBuilder,
        sample_rate: NonZeroU64,
    ) -> Result<Self> {
        inner_blobstores_scuba.add_common_server_data();
        multiplex_scuba.add_common_server_data();
        Ok(Self {
            inner_blobstores_scuba,
            multiplex_scuba,
            sample_rate,
        })
    }

    pub fn sampled(&mut self) {
        self.inner_blobstores_scuba.sampled(self.sample_rate);
        self.multiplex_scuba.sampled(self.sample_rate);
    }

    pub fn add_client_request_info(&mut self, ctx: &CoreContext) {
        if let Some(client_info) = ctx.client_request_info() {
            self.multiplex_scuba.add_client_request_info(client_info);
            self.inner_blobstores_scuba
                .add_client_request_info(client_info);
        }
    }
}

#[derive(Clone)]
pub struct WalMultiplexedBlobstore {
    /// Multiplexed blobstore configuration.
    pub(crate) multiplex_id: MultiplexId,
    /// Write-ahead log used to keep data consistent across blobstores.
    pub(crate) wal_queue: Arc<dyn BlobstoreWal>,

    pub(crate) quorum: MultiplexQuorum,
    /// These are the "normal" blobstores, which are read from on `get`, and written to on `put`
    /// as part of normal operation.
    pub(crate) blobstores: Arc<[(TimedStore, String)]>,
    /// Write-mostly blobstores are not normally read from on `get`, but take part in writes
    /// like a normal blobstore.
    pub(crate) write_only_blobstores: Arc<[(TimedStore, String)]>,

    /// Scuba table to log status of the underlying single blobstore queries.
    pub(crate) scuba: Scuba,

    /// Counter keeping track of the yet-to-complete blobstore operations in flight.
    pub(crate) inflight_ops_counter: Arc<AtomicU64>,
}

impl Drop for WalMultiplexedBlobstore {
    fn drop(&mut self) {
        // If there are any inflight get/put/is_present request to the blobstores,
        // wait a while before exiting. This is required to prevent the UAF issue
        // in Manifold. Reference: https://fburl.com/y5o44ed6
        if self.inflight_ops_counter.load(Ordering::Relaxed) > 0 {
            std::thread::sleep(std::time::Duration::from_secs(5))
        }
    }
}

impl std::fmt::Display for WalMultiplexedBlobstore {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "WAL MultiplexedBlobstore[normal {:?}, write only {:?}]",
            self.blobstores, self.write_only_blobstores
        )
    }
}

impl fmt::Debug for WalMultiplexedBlobstore {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "WalMultiplexedBlobstore: multiplex_id: {}",
            self.multiplex_id
        )?;
        f.debug_map()
            .entries(self.blobstores.iter().map(|(v, _)| (v.id(), v)))
            .finish()
    }
}

impl WalMultiplexedBlobstore {
    pub fn new(
        multiplex_id: MultiplexId,
        wal_queue: Arc<dyn BlobstoreWal>,
        blobstores: Vec<(BlobstoreId, Arc<dyn Blobstore>)>,
        write_only_blobstores: Vec<(BlobstoreId, Arc<dyn Blobstore>)>,
        write_quorum: usize,
        timeout: Option<MultiplexTimeout>,
        scuba: Scuba,
    ) -> Result<Self> {
        let quorum = MultiplexQuorum::new(blobstores.len(), write_quorum)?;

        let to = timeout.unwrap_or_default();

        let blobstore_id_strs: Vec<String> =
            blobstores.iter().map(|(id, _)| id.to_string()).collect();

        let write_only_blobstore_id_strs: Vec<String> = write_only_blobstores
            .iter()
            .map(|(id, _)| id.to_string())
            .collect();

        let timed_blobstores = with_timed_stores(blobstores, to.clone());
        assert_eq!(blobstore_id_strs.len(), timed_blobstores.len());

        let blobstores = zip(timed_blobstores, blobstore_id_strs)
            .collect::<Vec<_>>()
            .into();
        let write_only_blobstores = zip(
            with_timed_stores(write_only_blobstores, to),
            write_only_blobstore_id_strs,
        )
        .collect::<Vec<_>>()
        .into();

        let inflight_ops_counter = Arc::new(AtomicU64::new(0));
        Ok(Self {
            multiplex_id,
            wal_queue,
            blobstores,
            write_only_blobstores,
            quorum,
            scuba,
            inflight_ops_counter,
        })
    }

    /// `put` a key in the underlying blobstore.
    /// We will start a `put` operation in all underlying blobstores and return a success as soon
    /// as `quorum.write` of these operations were successful.
    /// If too many errors prevented us from reaching `quorum.write` successful puts in underlying
    /// blobstores, return an Error.
    async fn put_impl<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: Option<PutBehaviour>,
        scuba: &Scuba,
    ) -> Result<OverwriteStatus> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::BlobPuts);

        let blob_size = value.len() as u64;

        // Log the blobstore key and wait till it succeeds
        let ts = Timestamp::now();
        let log_entry = BlobstoreWalEntry::new(key.clone(), self.multiplex_id, ts, blob_size);
        let (stats, result) = self.wal_queue.log(ctx, log_entry).timed().await;

        scuba::record_queue_stats(
            ctx,
            &mut scuba.multiplex_scuba.clone(),
            &key,
            stats,
            None,
            self.to_string(),
            result.as_ref().map(|_| &()),
        );

        let entry = result.with_context(|| {
            format!("WAL Multiplexed Blobstore: Failed writing to the WAL: key {key}")
        })?;

        // Prepare underlying main blobstores puts
        let mut put_futs = inner_multi_put(
            ctx,
            self.blobstores.clone(),
            &key,
            &value,
            put_behaviour,
            scuba,
            self.inflight_ops_counter.clone(),
        );

        // Wait for the quorum successful writes
        let mut quorum: usize = self.quorum.write.get();
        let mut put_errors = HashMap::new();
        let (stats, result) = async move {
            while let Some(result) = put_futs.next().await {
                match result {
                    Ok(_overwrite_status) => {
                        quorum = quorum.saturating_sub(1);
                        if quorum == 0 {
                            // Quorum blobstore writes succeeded, we can spawn the rest
                            // of the writes and not wait for them.
                            let main_puts =
                                spawn_stream_completion(put_futs.map_err(|(_id, err)| err));

                            // Spawn the write-only blobstore writes, we don't want to wait for them
                            let write_only_puts = inner_multi_put(
                                ctx,
                                self.write_only_blobstores.clone(),
                                &key,
                                &value,
                                put_behaviour,
                                scuba,
                                self.inflight_ops_counter.clone(),
                            );
                            let write_only_puts =
                                spawn_stream_completion(write_only_puts.map_err(|(_id, err)| err));

                            cloned!(ctx, self.wal_queue);
                            if put_errors.is_empty() {
                                // Optimisation: It put fully succeeded on all blobstores, we can remove
                                // it from queue and healer doesn't need to deal with it.
                                mononoke::spawn_task(async move {
                                    let (r1, r2) = futures::join!(main_puts, write_only_puts);
                                    r1??;
                                    r2??;
                                    // TODO(yancouto): Batch deletes together.
                                    wal_queue.delete_by_key(&ctx, &[entry]).await?;
                                    anyhow::Ok(())
                                });
                            }

                            return Ok(OverwriteStatus::NotChecked);
                        }
                    }
                    Err((bs_id, err)) => {
                        put_errors.insert(bs_id, err);
                    }
                }
            }
            Err(put_errors)
        }
        .timed()
        .await;

        ctx.perf_counters().set_max_counter(
            PerfCounterType::BlobPutsMaxLatency,
            stats.completion_time.as_millis_unchecked() as i64,
        );
        ctx.perf_counters()
            .set_max_counter(PerfCounterType::BlobPutsMaxSize, blob_size as i64);
        ctx.perf_counters()
            .add_to_counter(PerfCounterType::BlobPutsTotalSize, blob_size as i64);

        result.map_err(|put_errors| {
            let errors = Arc::new(put_errors);
            let result_err = if errors.len() == self.blobstores.len() {
                // all main writes failed
                MultiplexedWalError::AllFailed(errors)
            } else {
                // some main writes failed
                MultiplexedWalError::SomePutsFailed(errors)
            };
            result_err.into()
        })
    }

    /// Unlink a key from this multiplexed blobstore.
    /// The operation is only considered successful if the key is absent from all underlying
    /// blobstores as any key remaining in any underlying blobstore could end up being healed back
    /// into the other blobstores.
    /// Since `put` will consider a write successful as soon as the key was written to
    /// `write_quorum` underlying blobstores, the key we want to unlink may be present in only some
    /// of the underlying blobstores at the time this function is called.
    /// For that reason, in `inner_multi_unlink`, if one `unlink` operation in an underlying
    /// blobstore fails but the key is absent as an outcome, the function returns `Ok(())`.
    /// This means that after calling `unlink_impl`, the key should be present in no underlying
    /// blobstore.
    /// Note: strictly, there is a race condition if someone tries to `put` a key at the same time
    /// as it's being `unlink`ed.
    /// To avoid this, we should hold a write-lock on all inner blobstores while unlinking, but
    /// this could be problematic.
    /// `unlink` should only be used in rare situations, and it is the caller's responsibility to
    /// ensure that no-one is attempting to `put` the same key during an `unlink` operation.
    async fn unlink_impl<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
        scuba: &Scuba,
    ) -> Result<()> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::BlobUnlinks);

        let blobstores: Arc<[(TimedStore, String)]> = self
            .blobstores
            .iter()
            .chain(self.write_only_blobstores.iter())
            .cloned()
            .collect();
        let num_blobstores = blobstores.len();

        let mut unlink_futs = inner_multi_unlink(
            ctx,
            blobstores,
            key,
            scuba,
            self.inflight_ops_counter.clone(),
        );

        // Unlink from all underlying blobstores
        let mut unlink_errors = HashMap::new();
        let (stats, result) = async move {
            while let Some(result) = unlink_futs.next().await {
                match result {
                    Ok(()) => {
                        // All good: we unlinked from this blobstore
                    }
                    Err((bs_id, err)) => {
                        // If the unlink failed and the key is still present, record the error and keep
                        // unlinking from other blobstores
                        unlink_errors.insert(bs_id, err);
                    }
                }
            }
            if unlink_errors.is_empty() {
                Ok(())
            } else {
                Err(unlink_errors)
            }
        }
        .timed()
        .await;

        ctx.perf_counters().set_max_counter(
            PerfCounterType::BlobUnlinksMaxLatency,
            stats.completion_time.as_millis_unchecked() as i64,
        );

        result.map_err(|unlink_errors| {
            let errors = Arc::new(unlink_errors);
            let result_err = if errors.len() == num_blobstores {
                // all unlinks failed
                MultiplexedWalError::AllFailed(errors)
            } else {
                // some unlinks failed
                MultiplexedWalError::SomeUnlinksFailed(errors)
            };
            result_err.into()
        })
    }

    /// `get` a key from this multiplexed blobstore
    /// We will query underlying blobstores until up to `quorum.read` of the queries were
    /// successful.
    /// * If  by that time, all queried blobstores didn't have the key in question, we
    ///   will consider this key absent and return `None`,
    /// * If any underlying blobstore has the key, return the output of its `get`,
    /// * If too many errors disable us from reaching one of the conditions above, propagate the
    ///   error to the caller.
    async fn get_impl<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
        scuba: &Scuba,
    ) -> Result<Option<(BlobstoreId, BlobstoreGetData)>> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::BlobGets);

        let mut get_futs = inner_multi_get(
            ctx,
            self.blobstores.clone(),
            key,
            OperationType::Get,
            scuba,
            self.inflight_ops_counter.clone(),
        );

        let num_blobstores_used = get_futs.len();

        // Wait for the quorum successful "Not Found" reads before
        // returning Ok(None).
        let mut quorum: usize = std::cmp::min(self.quorum.read.get(), num_blobstores_used);
        let mut get_errors = HashMap::with_capacity(get_futs.len());
        let (stats, result) = async move {
            while let Some((bs_id, result)) = get_futs.next().await {
                match result {
                    Ok(Some(get_data)) => {
                        return Ok(Some((bs_id, get_data)));
                    }
                    Ok(None) => {
                        quorum = quorum.saturating_sub(1);
                        if quorum == 0 {
                            // quorum blobstores couldn't find the given key in the blobstores
                            // let's trust them
                            return Ok(None);
                        }
                    }
                    Err(err) => {
                        get_errors.insert(bs_id, err);
                    }
                }
            }
            Err(get_errors)
        }
        .timed()
        .await;

        ctx.perf_counters().set_max_counter(
            PerfCounterType::BlobGetsMaxLatency,
            stats.completion_time.as_millis_unchecked() as i64,
        );

        let result = result.map_err(|get_errors| {
            let errors = Arc::new(get_errors);
            let result_err = if errors.len() == num_blobstores_used {
                // all main reads failed
                MultiplexedWalError::AllFailed(errors)
            } else {
                // some main reads failed
                MultiplexedWalError::SomeGetsFailed(errors)
            };
            result_err.into()
        });

        match result {
            Ok(Some((_bs_id, ref data))) => {
                ctx.perf_counters()
                    .set_max_counter(PerfCounterType::BlobGetsMaxSize, data.len() as i64);
                ctx.perf_counters()
                    .add_to_counter(PerfCounterType::BlobGetsTotalSize, data.len() as i64);
            }
            Ok(None) => {
                ctx.perf_counters()
                    .increment_counter(PerfCounterType::BlobGetsNotFound);
            }
            _ => {}
        }
        result
    }

    /// Is the key present in this multiplexed blobstore?
    /// * A key is considered `Present` if it can be found in any underlying blobstores by only
    ///   querying the first `quorum.read` that don't fail,
    /// * A key is considered `Absent` if it cannot be found in the first `quorum.read` underlying
    ///   blobstores we queried,
    /// * If the key is not found to be `Present` anywhere and the number of errors from underlying
    ///   blobstore prevents us from reaching the `quorum.read` to decide it is absent, return an
    ///   `Error`.
    async fn is_present_impl<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
        scuba: &Scuba,
    ) -> Result<BlobstoreIsPresent> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::BlobPresenceChecks);

        let mut futs = inner_multi_is_present(
            ctx,
            self.blobstores.clone(),
            key,
            scuba,
            self.inflight_ops_counter.clone(),
        );

        // Wait for the quorum successful "Not Found" reads before
        // returning Ok(None).
        let mut quorum: usize = self.quorum.read.get();
        let mut errors = HashMap::with_capacity(futs.len());
        let (stats, result) = async move {
            while let Some(result) = futs.next().await {
                match result {
                    (_, Ok(BlobstoreIsPresent::Present)) => {
                        return Ok(BlobstoreIsPresent::Present);
                    }
                    (_, Ok(BlobstoreIsPresent::Absent)) => {
                        quorum = quorum.saturating_sub(1);
                        // we return if there is either quorum on missing
                        if quorum == 0 {
                            return Ok(BlobstoreIsPresent::Absent);
                        }
                    }
                    (bs_id, Ok(BlobstoreIsPresent::ProbablyNotPresent(err))) => {
                        // Treat this like an error from the underlying blobstore.
                        // In reality, this won't happen as multiplexed operates over single
                        // standard blobstores, which always can answer if the blob is present.
                        errors.insert(bs_id, err);
                    }
                    (bs_id, Err(err)) => {
                        errors.insert(bs_id, err);
                    }
                }
            }
            Err(errors)
        }
        .timed()
        .await;

        ctx.perf_counters().set_max_counter(
            PerfCounterType::BlobPresenceChecksMaxLatency,
            stats.completion_time.as_millis_unchecked() as i64,
        );

        let errors = match result {
            Ok(is_present) => {
                return Ok(is_present);
            }
            Err(errs) => errs,
        };

        // At this point the multiplexed is_present either failed or cannot say for sure
        // if the blob is present:
        // - no blob was found, but some of the blobstore `is_present` calls failed
        // - there was no read quorum on "not found" result
        let errors = Arc::new(errors);
        if errors.len() == self.blobstores.len() {
            // all main reads failed -> is_present failed
            return Err(MultiplexedWalError::AllFailed(errors).into());
        }

        Ok(BlobstoreIsPresent::ProbablyNotPresent(
            MultiplexedWalError::SomeIsPresentsFailed(errors).into(),
        ))
    }
}

#[async_trait]
impl Blobstore for WalMultiplexedBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        let mut scuba = self.scuba.clone();
        scuba.sampled();
        scuba.add_client_request_info(ctx);
        let (stats, result) = self.get_impl(ctx, key, &scuba).timed().await;
        scuba::record_get(
            ctx,
            &mut scuba.multiplex_scuba,
            &self.multiplex_id,
            key,
            stats,
            &result,
        );
        Ok(result?.map(|(_, data)| data))
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        let mut scuba = self.scuba.clone();
        scuba.sampled();
        scuba.add_client_request_info(ctx);
        let (stats, result) = self.is_present_impl(ctx, key, &scuba).timed().await;
        scuba::record_is_present(
            ctx,
            &mut scuba.multiplex_scuba,
            &self.multiplex_id,
            key,
            stats,
            &result,
        );
        result
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        let mut scuba = self.scuba.clone();
        scuba.add_client_request_info(ctx);
        let size = value.len();
        let (stats, result) = self
            .put_impl(ctx, key.clone(), value, Some(put_behaviour), &scuba)
            .timed()
            .await;
        scuba::record_put(
            ctx,
            &mut scuba.multiplex_scuba,
            &self.multiplex_id,
            &key,
            size,
            stats,
            &result,
        );
        result
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        let size = value.len();
        let mut scuba = self.scuba.clone();
        scuba.add_client_request_info(ctx);
        let (stats, result) = self
            .put_impl(ctx, key.clone(), value, None, &scuba)
            .timed()
            .await;
        scuba::record_put(
            ctx,
            &mut scuba.multiplex_scuba,
            &self.multiplex_id,
            &key,
            size,
            stats,
            &result,
        );
        result
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        let mut scuba = self.scuba.clone();
        scuba.add_client_request_info(ctx);
        let (stats, result) = self.unlink_impl(ctx, key, &scuba).timed().await;
        scuba::record_unlink(
            ctx,
            &mut scuba.multiplex_scuba,
            &self.multiplex_id,
            key,
            stats,
            &result,
        );
        result
    }
}

fn spawn_stream_completion<T>(
    s: impl Stream<Item = Result<T>> + Send + 'static,
) -> JoinHandle<Result<()>> {
    mononoke::spawn_task(s.try_for_each(|_| future::ok(())))
}

fn inner_multi_put(
    ctx: &CoreContext,
    blobstores: Arc<[(TimedStore, String)]>,
    key: &str,
    value: &BlobstoreBytes,
    put_behaviour: Option<PutBehaviour>,
    scuba: &Scuba,
    counter: Arc<AtomicU64>,
) -> FuturesUnordered<impl Future<Output = Result<OverwriteStatus, (BlobstoreId, Error)>> + use<>> {
    let put_futs: FuturesUnordered<_> = blobstores
        .iter()
        .map(|(bs, _)| {
            // Note: the key used to be passed in as a `&String` and `cloned!` and that triggered
            // a clippy crash for some reason. See D44027145 for context.
            let key = key.to_string();
            cloned!(
                bs,
                ctx,
                value,
                put_behaviour,
                scuba.inner_blobstores_scuba,
                counter
            );
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                let result = bs
                    .put(&ctx, key, value, put_behaviour, inner_blobstores_scuba)
                    .await;
                counter.fetch_sub(1, Ordering::Relaxed);
                result
            }
        })
        .collect();
    put_futs
}

fn inner_multi_unlink<'a>(
    ctx: &'a CoreContext,
    blobstores: Arc<[(TimedStore, String)]>,
    key: &'a str,
    scuba: &Scuba,
    counter: Arc<AtomicU64>,
) -> FuturesUnordered<impl Future<Output = Result<(), (BlobstoreId, Error)>> + use<'a>> {
    let unlink_futs: FuturesUnordered<_> = blobstores
        .iter()
        .map(|(bs, _)| {
            cloned!(bs, ctx, scuba.inner_blobstores_scuba, counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                let result = bs.unlink(&ctx, key, inner_blobstores_scuba.clone()).await;
                if result.is_err() {
                    // The unlink operation could have failed because the key was missing from this
                    // underlying blobstore.
                    // This can happen as a key is considered written if it was written to
                    // `write_quorum` underlying blobstores, which might not be all of them.
                    // If we fail to unlink because a key is not present, we can safely treat that
                    // as a successful unlink.
                    if let (_, Ok(BlobstoreIsPresent::Absent)) =
                        bs.is_present(&ctx, key, inner_blobstores_scuba).await
                    {
                        return Ok(());
                    }
                }
                counter.fetch_sub(1, Ordering::Relaxed);
                result
            }
        })
        .collect();
    unlink_futs
}

pub(crate) type GetResult = (BlobstoreId, Result<Option<BlobstoreGetData>, Error>);

pub(crate) fn inner_multi_get<'a>(
    ctx: &'a CoreContext,
    blobstores: Arc<[(TimedStore, String)]>,
    key: &'a str,
    operation: OperationType,
    scuba: &Scuba,
    counter: Arc<AtomicU64>,
) -> FuturesUnordered<impl Future<Output = GetResult> + use<'a>> {
    let client_correlator = ctx
        .metadata()
        .client_info()
        .and_then(|ci| ci.request_info.as_ref().map(|cri| cri.correlator.as_str()));
    let get_futs: FuturesUnordered<_> = blobstores
        .iter()
        .filter(|(_bs, bs_id_str)| {
            // If the blobstore is temporarily disabled, don't create a future for it.
            !justknobs::eval(
                "scm/mononoke:disable_blobstore_reads",
                client_correlator,
                Some(bs_id_str),
            )
        })
        .map(|(bs, _)| {
            cloned!(bs, scuba.inner_blobstores_scuba, counter);
            async move {
                (*bs.id(), {
                    counter.fetch_add(1, Ordering::Relaxed);
                    let result = bs.get(ctx, key, operation, inner_blobstores_scuba).await;
                    counter.fetch_sub(1, Ordering::Relaxed);
                    result
                })
            }
        })
        .collect();

    get_futs
}

fn inner_multi_is_present<'a>(
    ctx: &'a CoreContext,
    blobstores: Arc<[(TimedStore, String)]>,
    key: &'a str,
    scuba: &Scuba,
    counter: Arc<AtomicU64>,
) -> FuturesUnordered<
    impl Future<Output = (BlobstoreId, Result<BlobstoreIsPresent, Error>)> + use<'a>,
> {
    let futs: FuturesUnordered<_> = blobstores
        .iter()
        .map(|(bs, _)| {
            cloned!(bs, scuba.inner_blobstores_scuba, counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                let result = bs.is_present(ctx, key, inner_blobstores_scuba).await;
                counter.fetch_sub(1, Ordering::Relaxed);
                result
            }
        })
        .collect();
    futs
}
