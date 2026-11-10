/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Journalled deletion of the keys recorded in an inventory.
//!
//! Each partition's unpacked keys are unlinked in fixed batches, a bounded
//! number of keys at a time across batches.  A batch's outcome is journalled
//! once all its keys are accounted for, so a restarted run redoes the batches
//! without one and retries only the keys that failed.  Packed keys are never
//! unlinked here: unlinking one would lose the other entries of its pack.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use clap::Args;
use context::CoreContext;
use futures::future;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use governor::DefaultDirectRateLimiter;
use governor::Jitter;
use governor::Quota;
use governor::RateLimiter;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::warn;

use crate::adapters::ComponentStore;
use crate::adapters::UnlinkOutcome;
use crate::scan::Cell;
use crate::scan::InventoryRecord;
use crate::scan::Sampled;
use crate::scan::StoredForm;
use crate::scan::partition_file;
use crate::state::Journal;
use crate::state::StateDir;

/// Unlinks allowed at once above the rate, as a fraction of it.
const RATE_LIMIT_BURST_DIVISOR: u32 = 10;

const RATE_LIMIT_JITTER: Duration = Duration::from_millis(5);

#[derive(Args, Clone, Debug)]
pub struct ApplyOptions {
    /// Keys unlinked per journalled batch.  A partition keeps the batch size
    /// it was started with
    #[clap(long, default_value_t = NonZeroUsize::new(1000).expect("1000 is non-zero"))]
    pub batch_size: NonZeroUsize,
    /// Unlinks in flight at once within a partition
    #[clap(long, default_value_t = NonZeroUsize::new(32).expect("32 is non-zero"))]
    pub unlink_concurrency: NonZeroUsize,
    /// Partitions processed at the same time
    #[clap(long, default_value_t = NonZeroUsize::new(32).expect("32 is non-zero"))]
    pub partition_concurrency: NonZeroUsize,
    /// Upper bound on unlinks per second for the whole process
    #[clap(long, default_value_t = NonZeroU32::new(5000).expect("5000 is non-zero"))]
    pub max_unlinks_per_second: NonZeroU32,
}

impl ApplyOptions {
    pub fn unlink_settings(&self) -> UnlinkSettings {
        UnlinkSettings::new(self.unlink_concurrency, self.max_unlinks_per_second)
    }
}

/// How keys are unlinked: at most `concurrency` at once, paced by one rate
/// shared by everything that unlinks.
pub struct UnlinkSettings {
    pub concurrency: NonZeroUsize,
    limiter: DefaultDirectRateLimiter,
}

impl UnlinkSettings {
    pub fn new(concurrency: NonZeroUsize, per_second: NonZeroU32) -> Self {
        let burst =
            NonZeroU32::new(per_second.get() / RATE_LIMIT_BURST_DIVISOR).unwrap_or(NonZeroU32::MIN);
        Self {
            concurrency,
            limiter: RateLimiter::direct(Quota::per_second(per_second).allow_burst(burst)),
        }
    }

    async fn until_ready(&self) {
        self.limiter
            .until_ready_with_jitter(Jitter::up_to(RATE_LIMIT_JITTER))
            .await;
    }
}

/// What applying one partition needs.
pub struct ApplyContext<'a> {
    pub ctx: &'a CoreContext,
    pub state: &'a StateDir,
    pub store: &'a ComponentStore,
    pub cell: Cell<'a>,
    pub batch_size: NonZeroUsize,
    pub unlink: &'a UnlinkSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedKey {
    pub key: String,
    pub error: String,
}

/// What happened to a set of keys being unlinked.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlinkCounts {
    pub unlinked: u64,
    pub already_absent: u64,
    pub failed: Vec<FailedKey>,
}

impl UnlinkCounts {
    fn record(&mut self, key: String, result: Result<UnlinkOutcome>) {
        match result {
            Ok(UnlinkOutcome::Unlinked) => self.unlinked += 1,
            Ok(UnlinkOutcome::AlreadyAbsent) => self.already_absent += 1,
            Err(e) => self.failed.push(FailedKey {
                key,
                error: format!("{e:#}"),
            }),
        }
    }

    fn add(&mut self, other: UnlinkCounts) {
        self.unlinked += other.unlinked;
        self.already_absent += other.already_absent;
        self.failed.extend(other.failed);
    }
}

/// The journalled outcome of one batch.  A batch with failed keys is
/// recorded again each time they are retried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct BatchDone {
    batch: usize,
    batch_size: usize,
    #[serde(flatten)]
    counts: UnlinkCounts,
}

/// What applying a partition's unpacked keys did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionOutcome {
    #[serde(flatten)]
    pub counts: UnlinkCounts,
    /// Packed keys, which cannot be unlinked without losing the other values
    /// of their packs.
    pub packed_deferred: u64,
    /// Keys still present that cannot be deleted safely, such as unreadable
    /// envelopes.
    pub blocked: Vec<String>,
}

fn journal_path(cell: &Cell<'_>, hex: &str) -> PathBuf {
    apply_dir(cell).join(partition_file(hex, ".journal.jsonl"))
}

fn done_path(cell: &Cell<'_>, hex: &str) -> PathBuf {
    apply_dir(cell).join(partition_file(hex, ".done.json"))
}

fn apply_dir(cell: &Cell<'_>) -> PathBuf {
    PathBuf::from("apply")
        .join(&cell.component.id)
        .join(cell.repo_id.id().to_string())
        .join(cell.recipe.id)
}

/// A batch recovered from a journal.
#[derive(Default)]
struct BatchState {
    unlinked: u64,
    already_absent: u64,
    /// The keys still failing after the batch last ran.
    remaining_failures: Vec<FailedKey>,
}

/// The batch size a journal was written with, if any, and its batches.
fn recover_batches(
    records: Vec<BatchDone>,
) -> Result<(Option<usize>, BTreeMap<usize, BatchState>)> {
    let batch_size = records.first().map(|record| record.batch_size);
    let mut batches: BTreeMap<usize, BatchState> = BTreeMap::new();
    for record in records {
        if Some(record.batch_size) != batch_size {
            bail!(
                "journal mixes batch sizes {} and {}",
                record.batch_size,
                batch_size.unwrap_or_default()
            );
        }
        let state = batches.entry(record.batch).or_default();
        state.unlinked += record.counts.unlinked;
        state.already_absent += record.counts.already_absent;
        state.remaining_failures = record.counts.failed;
    }
    Ok((batch_size, batches))
}

/// Unlinks one key, paced by the shared rate.
async fn unlink_key(
    ctx: &CoreContext,
    store: &ComponentStore,
    settings: &UnlinkSettings,
    key: &str,
) -> Result<UnlinkOutcome> {
    settings.until_ready().await;
    store.unlink(ctx, key).await
}

/// Unlinks the keys of `batches`, at most `actx.unlink.concurrency` at once
/// across batches, journalling each batch once all its keys are accounted
/// for.  Returns the counts of the batches journalled.
async fn unlink_batches(
    actx: &ApplyContext<'_>,
    journal: Journal,
    batch_size: usize,
    batches: Vec<(usize, Vec<String>)>,
) -> Result<UnlinkCounts> {
    // Keys left and counts so far of each batch still running.
    let running: StdMutex<HashMap<usize, (usize, UnlinkCounts)>> = StdMutex::new(
        batches
            .iter()
            .map(|(batch, keys)| (*batch, (keys.len(), UnlinkCounts::default())))
            .collect(),
    );
    // Held across each append, so records are written one at a time.
    let journal = Mutex::new(journal);
    let keys = batches
        .into_iter()
        .flat_map(|(batch, keys)| keys.into_iter().map(move |key| (batch, key)));
    stream::iter(keys)
        .map(|(batch, key)| {
            let (running, journal) = (&running, &journal);
            async move {
                let result = unlink_key(actx.ctx, actx.store, actx.unlink, &key).await;
                let finished = {
                    let mut running = running.lock().expect("lock poisoned");
                    let (left, counts) = running
                        .get_mut(&batch)
                        .expect("a batch runs until its last key is accounted for");
                    counts.record(key, result);
                    *left -= 1;
                    match *left {
                        0 => running.remove(&batch).map(|(_, counts)| counts),
                        _ => None,
                    }
                };
                let Some(counts) = finished else {
                    return Ok(UnlinkCounts::default());
                };
                journal
                    .lock()
                    .await
                    .append(&BatchDone {
                        batch,
                        batch_size,
                        counts: counts.clone(),
                    })
                    .await?;
                anyhow::Ok(counts)
            }
        })
        .buffer_unordered(actx.unlink.concurrency.get())
        .try_fold(UnlinkCounts::default(), |mut total, counts| async move {
            total.add(counts);
            Ok(total)
        })
        .await
}

/// Counts unreadable keys that are gone as already absent; the rest stay
/// blocked.
async fn check_blocked(actx: &ApplyContext<'_>, keys: Vec<String>) -> (u64, Vec<String>) {
    stream::iter(keys)
        .map(|key| async move {
            let absent = match actx.store.is_present(actx.ctx, &key).await {
                Ok(present) => !present,
                Err(e) => {
                    warn!("{key} stays blocked: {e:#}");
                    false
                }
            };
            (key, absent)
        })
        .buffer_unordered(actx.unlink.concurrency.get())
        .fold((0, Vec::new()), |(absent, mut blocked), (key, gone)| {
            if !gone {
                blocked.push(key);
            }
            future::ready((absent + u64::from(gone), blocked))
        })
        .await
}

/// Unlinks a partition's unpacked keys, resuming from its journal.
pub async fn apply_unpacked(actx: &ApplyContext<'_>, hex: &str) -> Result<PartitionOutcome> {
    let cell = &actx.cell;
    let inventory_path = cell.inventory_path(hex);
    if !actx.state.exists(&inventory_path).await? {
        bail!("{cell} partition {hex} has no inventory");
    }
    let records: Vec<InventoryRecord> = actx.state.read_json_lines(&inventory_path).await?;
    let mut unpacked = Vec::new();
    let mut unreadable = Vec::new();
    let mut packed_deferred = 0;
    for record in records {
        match record.form {
            StoredForm::Plain | StoredForm::Single => unpacked.push(record.key),
            StoredForm::Packed { .. } => packed_deferred += 1,
            StoredForm::Unreadable { .. } => unreadable.push(record.key),
            StoredForm::Vanished => {}
        }
    }
    let (absent_unreadable, blocked) = check_blocked(actx, unreadable).await;

    let journal_path = journal_path(cell, hex);
    let (journalled_size, mut recovered) =
        recover_batches(actx.state.read_journal(&journal_path).await?)?;
    let batch_size = journalled_size.unwrap_or(actx.batch_size.get());
    let mut counts = UnlinkCounts {
        already_absent: absent_unreadable,
        ..Default::default()
    };
    let mut todo = Vec::new();
    for (batch, keys) in unpacked.chunks(batch_size).enumerate() {
        match recovered.remove(&batch) {
            None => todo.push((batch, keys.to_vec())),
            Some(state) => {
                counts.unlinked += state.unlinked;
                counts.already_absent += state.already_absent;
                if !state.remaining_failures.is_empty() {
                    let retry = state.remaining_failures.into_iter().map(|f| f.key);
                    todo.push((batch, retry.collect()));
                }
            }
        }
    }
    let journal = actx.state.journal(&journal_path);
    counts.add(unlink_batches(actx, journal, batch_size, todo).await?);
    Ok(PartitionOutcome {
        counts,
        packed_deferred,
        blocked,
    })
}

/// The state of one partition after an apply pass.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionResult {
    pub outcome: PartitionOutcome,
}

impl PartitionResult {
    /// Every key is accounted for.
    pub fn done(&self) -> bool {
        self.outcome.counts.failed.is_empty()
            && self.outcome.blocked.is_empty()
            && self.outcome.packed_deferred == 0
    }
}

/// Applies one partition.  A finished partition is recorded and not
/// processed again.
pub async fn apply_partition(actx: &ApplyContext<'_>, hex: &str) -> Result<PartitionResult> {
    let done_path = done_path(&actx.cell, hex);
    if let Some(result) = actx.state.read_json(&done_path).await? {
        return Ok(result);
    }
    let result = PartitionResult {
        outcome: apply_unpacked(actx, hex).await?,
    };
    if result.done() {
        actx.state.write_json(&done_path, &result).await?;
    }
    Ok(result)
}

/// Totals over the partitions of one cell after an apply pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyTotals {
    pub partitions: usize,
    pub done: usize,
    pub unlinked: u64,
    pub already_absent: u64,
    pub failed: Sampled<FailedKey>,
    pub blocked: Sampled<String>,
    pub packed_deferred: u64,
}

impl ApplyTotals {
    /// Adds a partition whose apply pass returned `result`.
    pub fn record(&mut self, result: Option<&PartitionResult>) {
        self.partitions += 1;
        let Some(result) = result else {
            return;
        };
        self.done += usize::from(result.done());
        let outcome = &result.outcome;
        self.unlinked += outcome.counts.unlinked;
        self.already_absent += outcome.counts.already_absent;
        outcome
            .counts
            .failed
            .iter()
            .for_each(|failed| self.failed.note(failed.clone()));
        outcome
            .blocked
            .iter()
            .for_each(|blocked| self.blocked.note(blocked.clone()));
        self.packed_deferred += outcome.packed_deferred;
    }

    pub fn add(&mut self, other: &ApplyTotals) {
        self.partitions += other.partitions;
        self.done += other.done;
        self.unlinked += other.unlinked;
        self.already_absent += other.already_absent;
        self.failed.merge(&other.failed);
        self.blocked.merge(&other.blocked);
        self.packed_deferred += other.packed_deferred;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashSet;
    use std::fmt;
    use std::sync::Arc;

    use anyhow::anyhow;
    use async_trait::async_trait;
    use blobstore::Blobstore;
    use blobstore::BlobstoreEnumerationData;
    use blobstore::BlobstoreGetData;
    use blobstore::BlobstoreKeyParam;
    use blobstore::BlobstoreKeySource;
    use blobstore::OverwriteStatus;
    use blobstore::PutBehaviour;
    use fbinit::FacebookInit;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use mononoke_types::BlobstoreBytes;

    use super::*;
    use crate::policy::RECIPES;
    use crate::scan::InventoryContext;
    use crate::scan::inventory_partition;
    use crate::scan::tests::bytes;
    use crate::scan::tests::concurrency;
    use crate::scan::tests::hash;
    use crate::scan::tests::test_cell;
    use crate::scan::tests::test_plan;
    use crate::scan::tests::test_state;

    /// Settings shared by the tests' apply contexts.
    pub(crate) struct TestApply {
        unlink: UnlinkSettings,
    }

    impl TestApply {
        pub(crate) fn new(batch_size: usize) -> (Self, NonZeroUsize) {
            let this = Self {
                unlink: UnlinkSettings::new(
                    concurrency(2),
                    NonZeroU32::new(1_000_000).expect("non-zero"),
                ),
            };
            (this, concurrency(batch_size))
        }

        pub(crate) fn context<'a>(
            &'a self,
            ctx: &'a CoreContext,
            state: &'a StateDir,
            store: &'a ComponentStore,
            cell: Cell<'a>,
            batch_size: NonZeroUsize,
        ) -> ApplyContext<'a> {
            ApplyContext {
                ctx,
                state,
                store,
                cell,
                batch_size,
                unlink: &self.unlink,
            }
        }
    }

    /// A Memblob whose unlink fails once for chosen keys.
    #[derive(Debug)]
    struct FlakyBlob {
        inner: Memblob,
        fail_once: StdMutex<HashSet<String>>,
    }

    impl fmt::Display for FlakyBlob {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "FlakyBlob")
        }
    }

    #[async_trait]
    impl Blobstore for FlakyBlob {
        async fn get<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: &'a str,
        ) -> Result<Option<BlobstoreGetData>> {
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
            if self.fail_once.lock().expect("lock").remove(key) {
                return Err(anyhow!("injected failure"));
            }
            Blobstore::unlink(&self.inner, ctx, key).await
        }
    }

    #[async_trait]
    impl BlobstoreKeySource for FlakyBlob {
        async fn enumerate<'a>(
            &'a self,
            ctx: &'a CoreContext,
            range: &'a BlobstoreKeyParam,
        ) -> Result<BlobstoreEnumerationData> {
            self.inner.enumerate(ctx, range).await
        }
    }

    fn done(batch: usize, batch_size: usize, unlinked: u64, failed: Vec<FailedKey>) -> BatchDone {
        BatchDone {
            batch,
            batch_size,
            counts: UnlinkCounts {
                unlinked,
                already_absent: 0,
                failed,
            },
        }
    }

    #[mononoke::test]
    fn recovery_resumes_failed_batches_and_keeps_the_batch_size() -> Result<()> {
        let failed = FailedKey {
            key: "k3".to_string(),
            error: "e".to_string(),
        };
        let records = vec![done(1, 2, 1, vec![failed.clone()]), done(0, 2, 2, vec![])];
        let (batch_size, batches) = recover_batches(records)?;
        assert_eq!(batch_size, Some(2));
        assert!(batches[&0].remaining_failures.is_empty());
        assert_eq!(batches[&1].remaining_failures, vec![failed]);
        assert!(
            !batches.contains_key(&2),
            "a batch without a record is redone"
        );
        let retried = vec![done(1, 2, 1, vec![]), done(1, 2, 1, vec![])];
        let (_, batches) = recover_batches(retried)?;
        assert_eq!(batches[&1].unlinked, 2, "retries add to the batch's counts");
        assert!(
            recover_batches(vec![done(0, 2, 2, vec![]), done(1, 3, 3, vec![])]).is_err(),
            "mixed batch sizes would misalign batches"
        );
        assert_eq!(recover_batches(vec![])?.0, None);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn unpacked_keys_are_unlinked_and_retried(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blob = Arc::new(FlakyBlob {
            inner: Memblob::default(),
            fail_once: StdMutex::new(HashSet::new()),
        });
        let keys: Vec<String> = (0..5u8)
            .map(|n| format!("repo0001.historymf.file.blake2.0{}", &hash(n)[1..]))
            .collect();
        for key in &keys {
            blob.put(&ctx, key.clone(), bytes("v")).await?;
        }
        let survivor = format!("repo0001.fileunode.blake2.{}", hash(0));
        blob.put(&ctx, survivor.clone(), bytes("v")).await?;
        blob.fail_once.lock().expect("lock").insert(keys[3].clone());

        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let cell = test_cell(&plan, &RECIPES[0]);
        let store = ComponentStore::key_source(blob.clone());
        inventory_partition(
            InventoryContext {
                ctx: &ctx,
                state: &state,
                store: &store,
                concurrency: concurrency(1),
            },
            cell,
            "0",
        )
        .await?;
        // A key already gone before apply is accounted for, not a failure.
        Blobstore::unlink(&blob.inner, &ctx, &keys[4]).await?;

        let (test, batch_size) = TestApply::new(2);
        let actx = test.context(&ctx, &state, &store, cell, batch_size);
        let first = apply_unpacked(&actx, "0").await?;
        assert_eq!((first.counts.unlinked, first.counts.already_absent), (3, 1));
        assert_eq!(first.counts.failed.len(), 1);
        assert_eq!(first.counts.failed[0].key, keys[3]);
        let records: Vec<BatchDone> = state.read_journal(&journal_path(&cell, "0")).await?;
        assert_eq!(records.len(), 3, "one record per batch of two");

        assert!(
            apply_unpacked(&actx, "1").await.is_err(),
            "a partition without an inventory must not be applied"
        );
        assert!(
            !state.exists(&journal_path(&cell, "2")).await?,
            "journals are created by their first record"
        );

        // A rerun keeps the partition's batch size, whatever the option says.
        let (rerun, other_size) = TestApply::new(3);
        let actx = rerun.context(&ctx, &state, &store, cell, other_size);
        let second = apply_unpacked(&actx, "0").await?;
        assert_eq!(
            (second.counts.unlinked, second.counts.already_absent),
            (4, 1)
        );
        assert!(
            second.counts.failed.is_empty(),
            "the failed key was retried"
        );
        for key in &keys {
            assert!(!store.is_present(&ctx, key).await?);
        }
        assert!(store.is_present(&ctx, &survivor).await?);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn blocked_partitions_stay_unfinished_until_cleared(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let memblob = Arc::new(Memblob::default());
        let plain = format!("repo0001.historymf.file.blake2.{}", hash(0x01));
        let unreadable = format!("repo0001.historymf.file.blake2.{}.pack", hash(0x02));
        memblob.put(&ctx, plain.clone(), bytes("v")).await?;
        memblob.put(&ctx, unreadable.clone(), bytes("xx")).await?;

        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let cell = test_cell(&plan, &RECIPES[0]);
        let store = ComponentStore::key_source(memblob.clone());
        inventory_partition(
            InventoryContext {
                ctx: &ctx,
                state: &state,
                store: &store,
                concurrency: concurrency(1),
            },
            cell,
            "0",
        )
        .await?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(&ctx, &state, &store, cell, batch_size);

        let blocked = apply_partition(&actx, "0").await?;
        assert_eq!(blocked.outcome.blocked, vec![unreadable.clone()]);
        assert!(
            !blocked.done(),
            "an unreadable envelope blocks the partition"
        );
        assert!(!state.exists(&done_path(&cell, "0")).await?);
        assert!(store.is_present(&ctx, &unreadable).await?, "never deleted");

        // Removed by an operator, the envelope no longer blocks.
        memblob.unlink(&ctx, &unreadable).await?;
        let cleared = apply_partition(&actx, "0").await?;
        assert!(cleared.done());
        assert_eq!(cleared.outcome.counts.already_absent, 1);
        assert!(state.exists(&done_path(&cell, "0")).await?);

        // A done partition is not processed again.
        memblob.put(&ctx, plain.clone(), bytes("v")).await?;
        assert_eq!(apply_partition(&actx, "0").await?, cleared);
        assert!(store.is_present(&ctx, &plain).await?);
        Ok(())
    }
}
